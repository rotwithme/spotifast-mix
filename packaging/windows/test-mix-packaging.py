"""The custom fork must never claim or replace an upstream installation."""

import configparser
import importlib.util
import os
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from urllib.parse import unquote, urlsplit
import zipfile

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent


class ForkIsolationTests(unittest.TestCase):
    @unittest.skipUnless(os.name == "nt" and shutil.which("powershell"),
                         "requires the Windows portable packager")
    def test_portable_documentation_links_resolve_without_the_source_tree(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            # The packaging input needs only its version interface for this
            # documentation check. Native executable validation runs in CI too.
            binary = root / "version-probe.cmd"
            binary.write_text("@echo off\necho spotifast-mix 1.2.3\nexit /b 0\n")
            subprocess.run([
                "powershell", "-NoProfile", "-ExecutionPolicy", "Bypass",
                "-File", str(HERE / "package-mix.ps1"), "-NoBuild",
                "-BinaryPath", str(binary), "-OutputDirectory", str(root / "output"),
            ], check=True, capture_output=True, text=True)
            stem = "spotifast-mix-v1.2.3-x86_64-pc-windows-msvc"
            archive = root / f"output/{stem}.zip"
            with zipfile.ZipFile(archive) as package:
                # ZipInfo.filename normalizes Windows separators. The raw
                # name must also match .NET's exact bootstrap entry lookup.
                self.assertTrue(all("\\" not in entry.orig_filename
                                    for entry in package.infolist()))
                # The updater extracts this exact member, and every file is
                # inside that one version-and-target directory.
                entries = {name for name in package.namelist() if not name.endswith("/")}
                self.assertTrue(all(name.startswith(stem + "/") for name in entries))
                directories = {name for name in package.namelist() if name.endswith("/")}
                self.assertLessEqual(directories, {stem + "/"})
                names = {name.removeprefix(stem + "/") for name in entries}
                self.assertEqual(names, {
                    "spotifast-mix.exe", "LICENSE", "README.md", "Getting started.md",
                    "spotifast-mix-portable.txt",
                })
                for packaged, source in [
                    ("README.md", ROOT / "README.md"),
                    ("Getting started.md", ROOT / "docs/_reference/spotifast-mix.md"),
                ]:
                    with self.subTest(document=packaged):
                        text = package.read(stem + "/" + packaged).decode("utf-8").replace("\r\n", "\n")
                        # Packaging changes destinations only, preserving all
                        # instructions and link labels from the source guides.
                        link = r"\]\((?:<([^>]+)>|([^()]+))\)"
                        self.assertEqual(re.sub(link, "]()", text),
                                         re.sub(link, "]()", source.read_text(encoding="utf-8")))
                        for enclosed, plain in re.findall(link, text):
                            target = enclosed or plain
                            if urlsplit(target).scheme:
                                self.assertEqual(urlsplit(target).scheme, "https")
                            else:
                                self.assertIn(unquote(target.split("#", 1)[0]), names)

    def test_own_release_channel_does_not_enable_inherited_publishers(self):
        workflow = (ROOT / ".github/workflows/mix-release.yml").read_text(encoding="utf-8")
        self.assertIn("github.repository == 'rotwithme/spotifast-mix'", workflow)
        self.assertIn("MIX_UPDATE_SIGNING_KEY: ${{ secrets.MIX_UPDATE_SIGNING_KEY }}", workflow)
        self.assertIn("sign-mix-release.rb --verify", workflow)
        self.assertIn("--draft", workflow)
        self.assertIn("Refusing to replace an already published release.", workflow)
        self.assertNotIn("SPOTIFAST_UPDATE_SIGNING_KEY", workflow)
        self.assertNotIn("AZURE_", workflow)

    def test_windows_release_gate_keeps_every_non_mac_job_required(self):
        workflow = (ROOT / ".github/workflows/mix-release.yml").read_text(encoding="utf-8")
        required = re.search(r"\$required = @\(([^\n]+)\)", workflow).group(1)
        self.assertEqual(re.findall(r"'([^']+)'", required), [
            "policy", "docs", "scope", "quality", "test (windows-latest)",
            "test (windows-11-arm)", "test (ubuntu-latest)", "Nix package",
        ])
        self.assertIn("$ci.headSha -ne $commit", workflow)
        self.assertIn("$ci.conclusion -in @('cancelled'", workflow)
        self.assertIn("$job[0].status -ne 'completed'", workflow)
        self.assertIn("$job[0].conclusion -ne 'success'", workflow)
        self.assertIn("macOS (reported separately; not a Windows release gate)", workflow)

    @unittest.skipUnless(shutil.which("ruby"), "requires Ruby OpenSSL")
    def test_release_signature_binds_the_manifest_to_the_committed_public_key(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            # Disposable keys live only in this process and fixture. The
            # production signing key is never read by this regression.
            keygen = """require 'openssl'
key = OpenSSL::PKey.generate_key('ED25519')
File.binwrite(ARGV[0], key.private_to_pem)
File.write(ARGV[1], OpenSSL::ASN1.decode(key.public_to_der).value[1].value.unpack1('H*') + "\\n")
"""
            subprocess.run(["ruby", "-e", keygen, str(root / "dummy.pem"), str(root / "public.hex")],
                           check=True, capture_output=True)
            package = root / "spotifast-mix-v1.2.3-x86_64-pc-windows-msvc.zip"
            package.write_bytes(b"dummy portable release")
            import hashlib
            manifest = root / "checksums.txt"
            manifest.write_bytes((hashlib.sha256(package.read_bytes()).hexdigest()
                                  + "  " + package.name + "\n").encode())
            command = ["ruby", str(HERE / "sign-mix-release.rb")]
            env = dict(os.environ, MIX_UPDATE_SIGNING_KEY=(root / "dummy.pem").read_text())
            subprocess.run(command + [str(manifest), str(root / "public.hex")],
                           env=env, check=True, capture_output=True)
            self.assertEqual((root / "checksums.txt.sig").stat().st_size, 64)
            subprocess.run(command + ["--verify", str(manifest), str(root / "public.hex")],
                           check=True, capture_output=True)
            # A different committed key must never authorize signing.
            (root / "wrong.hex").write_text("00" * 32)
            failure = subprocess.run(command + [str(manifest), str(root / "wrong.hex")],
                                     env=env, capture_output=True, text=True)
            self.assertNotEqual(failure.returncode, 0)
            self.assertIn("does not match", failure.stderr)
            self.assertNotIn(env["MIX_UPDATE_SIGNING_KEY"], failure.stderr)
            package.write_bytes(b"changed bytes")
            failure = subprocess.run(command + ["--verify", str(manifest), str(root / "public.hex")],
                                     capture_output=True)
            self.assertNotEqual(failure.returncode, 0)

    def test_inherited_installer_builder_stops_before_touching_a_payload(self):
        spec = importlib.util.spec_from_file_location("mix_msi", HERE / "build-msi.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        with self.assertRaisesRegex(SystemExit, "portable ZIP only"):
            module.build(Path("missing-payload"), Path("must-not-exist.msi"), "1.2.3", "amd64")
        self.assertFalse(Path("must-not-exist.msi").exists())

    def test_direct_windows_installer_compilers_are_blocked_too(self):
        self.assertIn("<?error Spotifast Mix ships a portable ZIP only.",
                      (HERE / "spotifast.wxs").read_text(encoding="utf-8"))
        self.assertTrue((HERE / "spotifast.iss").read_text(encoding="utf-8")
                        .startswith("#error Spotifast Mix ships a portable ZIP only."))

    def test_linux_launcher_claims_only_the_fork_scheme(self):
        parser = configparser.ConfigParser(interpolation=None)
        parser.read(ROOT / "packaging/applications/spotifast.desktop", encoding="utf-8")
        entry = parser["Desktop Entry"]
        self.assertEqual(entry["Name"], "Spotifast Mix")
        self.assertEqual(entry["Exec"], "spotifast-mix %u")
        self.assertEqual(entry["MimeType"], "x-scheme-handler/spotifast-mix;")
        self.assertEqual(entry["StartupWMClass"], "spotifast-mix")

    def test_mac_bundle_and_login_agent_have_an_independent_identity(self):
        with (ROOT / "packaging/macos/Info.plist").open("rb") as stream:
            info = plistlib.load(stream)
        self.assertEqual(info["CFBundleDisplayName"], "Spotifast Mix")
        self.assertEqual(info["CFBundleURLTypes"][0]["CFBundleURLSchemes"], ["spotifast-mix"])
        with (ROOT / "packaging/macos/autostart.plist").open("rb") as stream:
            agent = plistlib.load(stream)
        self.assertEqual(agent["Label"], "app.spotifastmix.SpotifastMix.autostart")
        self.assertIn("app.spotifastmix.SpotifastMix", agent["ProgramArguments"])

    def test_nix_install_destinations_and_binary_are_isolated(self):
        recipe = (ROOT / "flake.nix").read_text(encoding="utf-8")
        for original in ["$out/bin/spotifast ", '$out/bin/spotifast"',
                         "$out/share/applications/spotifast.desktop",
                         "$out/share/icons/hicolor/scalable/apps/spotifast.svg",
                         "$out/share/spotifast/", "/Applications/Spotifast.app",
                         "rocks.spotifast.Spotifast", 'mainProgram = "spotifast"']:
            self.assertNotIn(original, recipe)
        self.assertIn('mainProgram = "spotifast-mix"', recipe)
        self.assertIn("$out/bin/spotifast-mix", recipe)
        self.assertIn("app.spotifastmix.SpotifastMix", recipe)

    def test_inherited_publication_jobs_are_explicitly_disabled(self):
        for name in ["release.yml", "packaging.yml", "docs.yml", "flatpak-from-release.yml"]:
            workflow = (ROOT / ".github/workflows" / name).read_text(encoding="utf-8")
            jobs = workflow.split("\njobs:\n", 1)[1]
            starts = list(re.finditer(r"^  ([A-Za-z0-9_-]+):\n", jobs, re.MULTILINE))
            self.assertTrue(starts, name)
            for start in starts:
                with self.subTest(workflow=name, job=start[1]):
                    self.assertEqual(jobs[start.end():].splitlines()[0],
                                     "    if: ${{ false }}")


if __name__ == "__main__":
    unittest.main()
