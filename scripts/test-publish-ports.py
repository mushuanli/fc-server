#!/usr/bin/env python3
"""Publication validation, NAT scope and interrupted transaction regression tests."""
from importlib.util import module_from_spec, spec_from_file_location
import ipaddress
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = spec_from_file_location('publisher', Path(__file__).with_name('publish-ports.py'))
publisher = module_from_spec(SPEC)
SPEC.loader.exec_module(publisher)


def record(listen='127.0.0.1', protocol='tcp', port=18080, instance='old'):
    return {'listen': listen, 'protocol': protocol, 'host_port': port, 'guest_port': 8080,
            'id': 'vm', 'instance': instance, 'slot': 0, 'token': 0xfc000001}


class PublicationTests(unittest.TestCase):
    def setUp(self):
        guest, uplink = map(ipaddress.ip_network, ('10.203.0.0/16', '10.204.0.0/16'))
        self.item = publisher.network.slot(0, guest, uplink)
        self.plan = {'slots': [self.item], 'egress_interface': 'eth0'}

    def test_loopback_never_adds_an_ingress_dnat_rule(self):
        rules = publisher.root_rules([record()], self.plan)
        self.assertIn('published_output ip daddr 127.0.0.1 tcp dport 18080', rules)
        self.assertNotIn('add rule ip mindos_firecracker_nat published_prerouting', rules)
        self.assertIn('snat to 10.204.0.1', rules)

    def test_wildcard_ingress_is_scoped_to_host_addresses(self):
        rules = publisher.root_rules([record('0.0.0.0', 'udp')], self.plan)
        self.assertIn('published_prerouting ip daddr != 127.0.0.0/8 fib daddr type local udp', rules)
        base = publisher.network.host_rules(self.plan)
        self.assertIn('iifname != "mfc-h*" jump published_prerouting', base)
        self.assertLess(base.index('jump published_guard'), base.index('ct state established,related accept'))

    def test_wildcard_conflict_and_protocol_independence(self):
        request = {'id': 'other', 'instance': 'new', 'slot': 0, 'spec': {
            'listen': '127.0.0.1', 'protocol': 'tcp', 'host_port': 18080, 'guest_port': 8081}}
        with self.assertRaisesRegex(publisher.Failure, 'HOST_PORT_IN_USE'):
            publisher.add([record('0.0.0.0')], request)
        request['spec']['protocol'] = 'udp'
        self.assertEqual(len(publisher.add([record('0.0.0.0')], request)), 2)

    def test_same_instance_retry_is_idempotent(self):
        existing = record()
        request = {key: existing[key] for key in ('id', 'instance', 'slot')}
        request['spec'] = {key: existing[key] for key in ('listen', 'protocol', 'host_port', 'guest_port')}
        self.assertEqual(publisher.add([existing], request), [existing])

    def test_invalid_input_cannot_inject_nft_statements(self):
        for value in (True, 0, -1, 65536):
            spec = {key: record()[key] for key in ('listen', 'host_port', 'guest_port', 'protocol')}
            spec['host_port'] = value
            with self.assertRaises(publisher.Failure):
                publisher.specification(spec)

    def test_new_mapping_purges_prior_untranslated_udp_flow(self):
        with patch.object(publisher, 'conntrack_delete') as delete:
            publisher.purge_listener(record(protocol='udp'))
        self.assertIn('--orig-dst', delete.call_args.args[0])
        self.assertIn('127.0.0.1', delete.call_args.args[0])
        self.assertIn('18080', delete.call_args.args[0])

    def test_only_changed_namespaces_are_rebuilt(self):
        second = {**record(port=18081), 'slot': 1, 'token': 0xfc000002}
        self.assertEqual(publisher.changed_slots([record(), second], [second]), {0})

    def test_instance_identity_prevents_old_clear_from_removing_new_mapping(self):
        request = {'action': 'clear', 'id': 'vm', 'instance': 'old', 'slot': 0}
        with patch.object(publisher, 'slot_for', return_value=self.item), patch.object(publisher, 'transaction') as change:
            publisher.change(request, [record(instance='new')], self.plan)
        self.assertEqual(change.call_args.args[1], [record(instance='new')])

    def test_apply_failure_restores_committed_state(self):
        with tempfile.TemporaryDirectory() as directory:
            ledger = Path(directory) / 'ports.json'
            with patch.object(publisher.network.installer, 'trusted'), patch.object(publisher, 'LEDGER', ledger), patch.object(publisher, 'purge'), \
                    patch.object(publisher, 'apply', side_effect=[publisher.Failure('NFT_PUBLICATION_APPLY_FAILED'), None]) as apply:
                with self.assertRaises(publisher.Failure):
                    publisher.transaction([], [record()], self.plan)
            self.assertEqual(json.loads(ledger.read_text()), {'records': []})
            self.assertEqual(apply.call_args.args[0], [])

    def test_failed_rollback_revokes_marks_and_keeps_recovery_journal(self):
        with tempfile.TemporaryDirectory() as directory:
            ledger = Path(directory) / 'ports.json'
            with patch.object(publisher.network.installer, 'trusted'), patch.object(publisher, 'LEDGER', ledger), patch.object(publisher, 'apply', side_effect=OSError()), \
                    patch.object(publisher, 'nft') as nft:
                with self.assertRaises(OSError):
                    publisher.transaction([], [record()], self.plan)
            nft.assert_called_once_with('flush set inet mindos_firecracker published_marks\n')
            self.assertEqual(json.loads(ledger.read_text())['pending'], [record()])


if __name__ == '__main__':
    unittest.main()
