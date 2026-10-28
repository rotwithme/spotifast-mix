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
            archive = root / "output/spotifast-mix-v1.2.3-windows-portable.zip"
            with zipfile.ZipFile(archive) as package:
                names = set(package.namelist())
                self.assertEqual(names, {
                    "spotifast-mix.exe", "LICENSE", "README.md", "Getting started.md",
                    "spotifast-mix-portable.txt",
                })
                for packaged, source in [
                    ("README.md", ROOT / "README.md"),
                    ("Getting started.md", ROOT / "docs/_reference/spotifast-mix.md"),
                ]:
                    with self.subTest(document=packaged):
                        text = package.read(packaged).decode("utf-8").replace("\r\n", "\n")
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
