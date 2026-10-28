"""Exercise native and Flatpak launcher installs using a release payload.

Uses the actual AUR package() and Flatpak install commands, without compiling or
launching the app. YAML is read through Ruby or an optional local PyYAML install.
"""
import configparser
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent


def yaml(path):
    if not shutil.which("ruby"):
        try:
            import yaml as yaml_module
        except ImportError:
            raise RuntimeError("Launcher checks need Ruby or PyYAML for YAML parsing") from None
        return yaml_module.safe_load(path.read_text(encoding="utf-8"))
    return json.loads(subprocess.check_output([
        "ruby", "-rjson", "-ryaml", "-e",
        "puts JSON.generate(YAML.safe_load_file(ARGV.fetch(0), aliases: false))", str(path),
    ], text=True))


def payload(root):
    (root / "packaging/applications").mkdir(parents=True)
    (root / "packaging/icons").mkdir(parents=True)
    shutil.copyfile(ROOT / "packaging/applications/spotifast.desktop",
                    root / "packaging/applications/spotifast.desktop")
    shutil.copyfile(ROOT / "packaging/icons/spotifast.svg", root / "packaging/icons/spotifast.svg")
    (root / "target/release").mkdir(parents=True)
    for name in ["spotifast-mix", "target/release/spotifast-mix"]:
        shutil.copyfile(shutil.which("true"), root / name)
    for name in ["LICENSE", "README.md"]:
        shutil.copyfile(ROOT / name, root / name)
    shutil.copytree(ROOT / "contrib/omarchy", root / "contrib/omarchy")
    (root / "packaging/flatpak").mkdir()
    shutil.copyfile(ROOT / "packaging/flatpak/app.spotifastmix.SpotifastMix.metainfo.xml",
                    root / "packaging/flatpak/app.spotifastmix.SpotifastMix.metainfo.xml")


class LauncherInstallTest(unittest.TestCase):
    def test_generated_aur_versions_start_at_the_requested_release(self):
        for package in ["spotifast-mix", "spotifast-mix-bin", "spotifast-mix-git"]:
            with self.subTest(package=package), tempfile.TemporaryDirectory() as directory:
                recipe = Path(directory) / "PKGBUILD"
                template = (ROOT / f"packaging/arch/{package.replace('spotifast-mix', 'spotifast')}/PKGBUILD.in").read_text()
                recipe.write_text(template.replace("@VERSION@", "9.8.7"))
                version = subprocess.check_output([
                    "bash", "-c", 'source "$1"; printf "%s" "$pkgver"', "check", str(recipe),
                ], text=True)
                self.assertEqual(version, "9.8.7")

    def check_launcher(self, prefix, desktop, window_class, icon):
        entries = list((prefix / "share/applications").glob("*.desktop"))
        self.assertEqual([p.name for p in entries], [desktop + ".desktop"])
        parser = configparser.ConfigParser(interpolation=None)
        parser.read(entries[0])
        entry = parser["Desktop Entry"]
        self.assertEqual(entry["Name"], "Spotifast Mix")
        self.assertEqual(entry["Icon"], icon)
        self.assertEqual(entry["StartupWMClass"], window_class)
        self.assertEqual(entry["Exec"], "spotifast-mix %u")
        self.assertEqual(entry["MimeType"], "x-scheme-handler/spotifast-mix;")
        icons = list((prefix / "share/icons/hicolor/scalable/apps").glob("*.svg"))
        self.assertEqual([p.name for p in icons], [icon + ".svg"])
        self.assertEqual(icons[0].read_bytes(), (ROOT / "packaging/icons/spotifast.svg").read_bytes())
        self.assertFalse((prefix / "bin/spotifast-mix").is_symlink())
        self.assertTrue((prefix / "bin/spotifast-mix").is_file())
        self.assertEqual([p.name for p in (prefix / "bin").iterdir()], ["spotifast-mix"])

    def test_aur_payloads_install_the_launcher(self):
        for package in ["spotifast-mix", "spotifast-mix-bin", "spotifast-mix-git"]:
            with self.subTest(package=package), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                if package.endswith("-bin"):
                    relative = "spotifast-mix-v9.8.7-x86_64-unknown-linux-gnu"
                elif package.endswith("-git"):
                    relative = package
                else:
                    relative = "spotifast-mix-9.8.7"
                payload(root / "src" / relative)
                template = ROOT / f"packaging/arch/{package.replace('spotifast-mix', 'spotifast')}/PKGBUILD.in"
                command = "source " + shlex.quote(template.as_posix()) + "; pkgver=9.8.7; package"
                subprocess.run(["bash", "-euc", command], check=True, cwd=root, env={
                    **os.environ, "CARCH": "x86_64", "srcdir": (root / "src").as_posix(),
                    "pkgdir": (root / "pkg").as_posix(),
                })
                self.check_launcher(root / "pkg/usr", "spotifast-mix", "spotifast-mix", "spotifast-mix")

    def test_flatpak_install_commands_keep_app_id_and_icon_aligned(self):
        for manifest in ["app.spotifastmix.SpotifastMix.yml", "app.spotifastmix.SpotifastMix.bundle.yml"]:
            config = yaml(ROOT / "packaging/flatpak" / manifest)
            commands = config["modules"][-1]["build-commands"]
            with self.subTest(manifest=manifest), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                payload(root / "source")
                # Exercise installation only. Cargo build coverage belongs to
                # the app suites; this fixture supplies the completed input.
                commands_to_run = [c for c in commands if not c.startswith("cargo ")]
                command = "\n".join(c.replace("/app/", shlex.quote((root / "app").as_posix()) + "/") for c in commands_to_run)
                subprocess.run(["bash", "-euc", command], check=True, cwd=root / "source")
                self.check_launcher(root / "app", config["id"], config["id"], config["id"])

    def test_native_manifest_installs_the_launcher_and_icon(self):
        config = yaml(ROOT / "native-packages.yaml")
        entries = [c for c in config["nfpm"]["contents"] if "/packaging/" in c["src"]]
        self.assertEqual(len(entries), 2)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload(root / "spotifast-mix-v9.8.7-linux-amd64")
            for entry in entries:
                pattern = entry["src"].replace("@PAYLOAD@/", "").replace("@VERSION@", "9.8.7").replace("@TARGET@", "linux-amd64")
                files = list(root.glob(pattern))
                self.assertEqual(len(files), 1)
                self.assertEqual(files[0].stem, "spotifast")
                self.assertTrue(entry["dst"].endswith("/"))


if __name__ == "__main__":
    unittest.main()
