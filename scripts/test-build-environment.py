#!/usr/bin/env python3
"""Deployment regression tests; require neither root nor a running VM."""
from importlib.util import module_from_spec, spec_from_file_location
from pathlib import Path
from collections import namedtuple
import ipaddress
import os
import stat
import tempfile
import tomllib
import unittest
from unittest.mock import patch


SPEC = spec_from_file_location("installer", Path(__file__).with_name("install-environment.py"))
installer = module_from_spec(SPEC)
SPEC.loader.exec_module(installer)
REPO = Path(__file__).resolve().parents[1]
INFO_SPEC = spec_from_file_location('image_info', REPO / 'scripts/image-info.py')
image_info = module_from_spec(INFO_SPEC)
INFO_SPEC.loader.exec_module(image_info)
NET_SPEC = spec_from_file_location('network_info', REPO / 'scripts/network-environment.py')
network_info = module_from_spec(NET_SPEC)
NET_SPEC.loader.exec_module(network_info)
SHARED_SPEC = spec_from_file_location('shared_root', REPO / 'scripts/shared-root-environment.py')
shared_root = module_from_spec(SHARED_SPEC)
SHARED_SPEC.loader.exec_module(shared_root)


class DeploymentTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)

    def test_config_and_key_survive_reinstall(self):
        with patch.object(installer, "trusted"):
            installer.initial_config(self.directory, REPO, Path("/usr/local/libexec/firecracker-virtiofs"))
            environment = self.directory / "environment"
            original_key = environment.read_bytes()
            config = self.directory / "config.toml"
            self.assertIn('virtiofsd = "/usr/local/libexec/firecracker-virtiofs/virtiofsd"', config.read_text())
            config.write_text("# administrator config\n")
            installer.initial_config(self.directory, REPO, Path("/another/daemon"))
            self.assertEqual(config.read_text(), "# administrator config\n")
            self.assertEqual(environment.read_bytes(), original_key)
            self.assertEqual(stat.S_IMODE(environment.stat().st_mode), 0o600)

    def test_failed_copy_preserves_existing_artifact(self):
        target = self.directory / "rootfs.ext4"
        target.write_bytes(b"previous image")
        with patch.object(installer, "trusted"), self.assertRaises(FileNotFoundError):
            installer.atomic_file(target, 0o600, source=self.directory / "missing")
        self.assertEqual(target.read_bytes(), b"previous image")
        self.assertEqual(list(self.directory.iterdir()), [target])

    def test_atomic_replacement_is_private(self):
        target = self.directory / "environment"
        with patch.object(installer, "trusted"):
            installer.atomic_file(target, 0o600, data="private test fixture")
        self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o600)
        self.assertEqual(target.read_text(), "private test fixture")

    def test_sparse_image_survives_deployment(self):
        source, target = self.directory / 'source.ext4', self.directory / 'deployed.ext4'
        size = 64 * 1024 * 1024
        with source.open('wb') as output:
            output.write(b'head')
            output.seek(size - 1)
            output.write(b'Z')
        with patch.object(installer, 'trusted'):
            installer.atomic_file(target, 0o600, source=source)
        self.assertEqual(target.stat().st_size, size)
        self.assertLess(target.stat().st_blocks * 512, size // 2)
        with target.open('rb') as content:
            self.assertEqual(content.read(4), b'head')
            content.seek(size - 1)
            self.assertEqual(content.read(), b'Z')

    def test_trust_rejects_symlink(self):
        link = self.directory / "alias"
        link.symlink_to(self.directory)
        with self.assertRaisesRegex(RuntimeError, "symlink"):
            installer.trusted(link)

    def test_existing_binding_ownership_is_preserved(self):
        with patch.object(installer.os, "chown") as chown:
            installer.new_binding(self.directory, 0o700)
            chown.assert_not_called()

    def test_binding_symlink_is_rejected(self):
        link = self.directory / "project"
        link.symlink_to(self.directory)
        with self.assertRaisesRegex(RuntimeError, "symlink"):
            installer.new_binding(link, 0o700)

    def test_custom_uid_range_conflict_is_rejected(self):
        account = namedtuple("Account", "pw_name pw_uid")("occupied", 300003)
        with patch.object(installer.pwd, "getpwall", return_value=[account]), \
                patch.object(installer.grp, "getgrall", return_value=[]), \
                self.assertRaisesRegex(RuntimeError, "occupied"):
            installer.reserve_uids(300000, 4)


class SharedRootTests(unittest.TestCase):
    def test_migration_preserves_network_and_is_idempotent(self):
        text = (REPO / 'config.virtiofs.example.toml').read_text()
        text += '\n[[profiles.debian13.network_slots]]\ntap="fc-tap"\nnetns="/run/netns/a"\nguest_mac="02:fc:00:00:00:01"\n'
        before = tomllib.loads(text)
        updated = shared_root.updated(text, 'debian13')
        self.assertEqual(shared_root.updated(updated, 'debian13'), updated)
        after = tomllib.loads(updated)
        self.assertEqual(before['bindings'], after['bindings'])
        self.assertEqual(before['profiles']['debian13']['network_slots'],
                         after['profiles']['debian13']['network_slots'])
        self.assertEqual(after['profiles']['debian13']['rootfs_mode'], 'shared')

    def test_migration_rejects_custom_images(self):
        text = (REPO / 'config.virtiofs.example.toml').read_text()
        text = text.replace('/var/lib/mindos-images/debian13.ext4', '/other.ext4')
        with self.assertRaisesRegex(RuntimeError, 'built-in Debian image'):
            shared_root.updated(text, 'debian13')


class ImageRecipeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name)
        (self.repo / 'guest').mkdir()
        (self.repo / 'scripts').mkdir()
        (self.repo / 'guest/Dockerfile').write_text('FROM debian:13-slim\n')
        (self.repo / 'scripts/build-debian13.sh').write_text('# image export recipe\n')
        (self.repo / 'scripts/build-initramfs.py').write_text('# bootstrap archive recipe\n')

    def test_guest_change_invalidates_cached_image(self):
        before = image_info.inputs(self.repo, 2048, 'base')
        (self.repo / 'guest/service').write_text('new startup service')
        self.assertNotEqual(before, image_info.inputs(self.repo, 2048, 'base'))

    def test_disk_size_and_flavor_invalidate_cached_image(self):
        before = image_info.inputs(self.repo, 2048, 'base')
        self.assertNotEqual(before, image_info.inputs(self.repo, 4096, 'base'))
        self.assertNotEqual(before, image_info.inputs(self.repo, 2048, 'harness'))

    def test_local_dependencies_do_not_change_recipe(self):
        before = image_info.inputs(self.repo, 2048, 'base')
        dependencies = self.repo / 'guest/node_modules'
        dependencies.mkdir()
        (dependencies / 'local-package').write_text('not part of Docker context')
        self.assertEqual(before, image_info.inputs(self.repo, 2048, 'base'))

    def test_private_ssh_key_cannot_be_baked_into_image(self):
        key = self.repo / 'private-key'
        key.write_text('-----BEGIN OPENSSH PRIVATE KEY-----\nprivate-fixture\n')
        with patch.dict(os.environ, {'SSH_PUBLIC_KEY': str(key)}), self.assertRaises(ValueError):
            image_info.ssh_key()


class NetworkPlanTests(unittest.TestCase):
    def test_slots_have_disjoint_addresses_and_mac_addresses(self):
        guest, uplink = map(ipaddress.ip_network, ('10.203.0.0/16', '10.204.0.0/16'))
        slots = [network_info.slot(i, guest, uplink) for i in range(100)]
        self.assertEqual(len({item['guest_mac'] for item in slots}), 100)
        self.assertEqual(len({item['guest_ip'] for item in slots}), 100)
        networks = [ipaddress.ip_network(item['guest_subnet']) for item in slots]
        for index, first in enumerate(networks):
            self.assertFalse(any(first.overlaps(second) for second in networks[index + 1:]))

    def test_loopback_dns_is_rejected(self):
        with patch.dict(os.environ, {'FC_DNS_SERVERS': '127.0.0.53'}), self.assertRaises(RuntimeError):
            network_info.dns_servers()

    def test_existing_host_routes_cannot_be_overridden(self):
        plan = {'guest_network': '10.203.0.0/16', 'uplink_network': '10.204.0.0/16'}
        response = namedtuple('Response', 'stdout')('[{"dst":"10.0.0.0/8","dev":"corp"}]')
        with patch.object(network_info, 'run', return_value=response), self.assertRaises(RuntimeError):
            network_info.check_routes(plan)


if __name__ == "__main__":
    unittest.main()
