#!/usr/bin/env python3
"""Regenerates the corpus under libarchive/tests/corpus: generated/, vendor/ harvest, manifest.json.

Reproducible from the repository: python3 stdlib plus the CLI tools on PATH (7z, zip, tar, gzip,
bzip2, xz, zstd) plus cargo for the handful of codecs no CLI here can write (lz4, brotli, LZH,
RAR — see examples/gen_corpus.rs, which this script runs).

Determinism contract: every plaintext is a fixed constant and every staged file is stamped with
FIXED_MTIME, so unencrypted archives regenerate byte-for-byte. Two families cannot, by format
design: encrypted archives carry a random salt (zip -P, 7z -p), and the LZH fixtures are written
by oxiarc's LzhWriter, which stamps the current time into every header. For those, regeneration
changes the archive bytes and nothing else — the entry ground truth in the manifest is computed
from the plaintext and never moves.

The manifest is the contract; the archive files are its evidence. Entry ground truth (name, size,
crc32) is computed from the *plaintext* the archives were made from, never by reading an archive
back, so a bug that every reader shares cannot hide in the corpus.

Run:  python3 libarchive/tests/corpus/generate.py
"""

import hashlib
import json
import os
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import zipfile
import zlib
from pathlib import Path

CORPUS = Path(__file__).resolve().parent
REPO = CORPUS.parent.parent.parent
GENERATED = CORPUS / "generated"
VENDOR = CORPUS / "vendor"
MANIFEST = CORPUS / "manifest.json"

# 2000-01-01T00:00:00Z. Every staged file, every tar header and every zip header carries this
# timestamp, so the archive bytes do not depend on when the generator ran.
FIXED_MTIME = 946684800
FIXED_ZIP_DATE = (2000, 1, 1, 0, 0, 0)

# ---------------------------------------------------------------------------
# The plaintext every generated archive is made from. Fixed constants, so a
# regeneration on another machine produces the same entry CRCs.
# ---------------------------------------------------------------------------

def hello() -> bytes:
    return b"Hello, libarchive-rs!\n"


def data_bin() -> bytes:
    return bytes((i * 31 + 7) % 256 for i in range(4096))


def empty() -> bytes:
    return b""


def inner() -> bytes:
    return b"nested entry\n"


# name -> plaintext. Order is index order everywhere.
PLAINTEXT = [
    ("hello.txt", hello),
    ("data.bin", data_bin),
    ("empty.txt", empty),
    ("nested/inner.txt", inner),
]


def entries_of(files=PLAINTEXT):
    return [
        {"name": name, "size": len(content()), "crc32": zlib.crc32(content()) & 0xFFFFFFFF, "dir": False}
        for name, content in files
    ]


def stage(dirpath: Path, files=PLAINTEXT):
    """Writes the plaintext tree for the CLI tools, stamped with the fixed timestamp."""
    for name, content in files:
        path = dirpath / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content())
        os.utime(path, (FIXED_MTIME, FIXED_MTIME))
    for directory in [dirpath, *dirpath.rglob("*")]:
        if directory.is_dir() and not directory.is_symlink():
            os.utime(directory, (FIXED_MTIME, FIXED_MTIME))


def zinfo(name: str) -> zipfile.ZipInfo:
    """A ZipInfo with the fixed timestamp, so python's zipfile output is byte-reproducible."""
    return zipfile.ZipInfo(name, date_time=FIXED_ZIP_DATE)


def run(argv, **kwargs):
    """Runs a CLI tool, failing loudly with the command line for the manifest."""
    subprocess.run(argv, check=True, **kwargs)
    return " ".join(argv)


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


# ---------------------------------------------------------------------------
# Manifest assembly. Every record says how the file was made.
# ---------------------------------------------------------------------------

FILES = []
MALFORMED = []


def add_file(path: str, format_: str, made_with: str, *, status="gate", owner=None,
             password=None, entries=None, oracles=(), notes="", expect_kind="ok",
             error_kind=None):
    FILES.append({
        "path": path,
        "format": format_,
        "status": status,
        "owner": owner,
        "password": password,
        "made_with": made_with,
        "expect": {"kind": expect_kind, "error_kind": error_kind},
        "entries": entries,
        "oracles": list(oracles),
        "notes": notes,
    })


def add_malformed(path: str, case: str, format_: str, *, expect="err", error_kind=None, notes=""):
    MALFORMED.append({
        "path": path,
        "case": case,
        "format": format_,
        "expect": expect,
        "error_kind": error_kind,
        "notes": notes,
    })


# ---------------------------------------------------------------------------
# Vendor harvest: real archives from unarc-rs 0.7.2's own test corpus.
# ---------------------------------------------------------------------------

# (destination under vendor/unarc-rs-0.7.2/, source under the unarc-rs tests/ tree)
HARVEST = [
    ("arc/store.arc", "arc/store.arc"),
    ("arc/crunch.arc", "arc/crunch.arc"),
    ("arc/license_cypted.arc", "arc/license_cypted.arc"),
    ("arj/stored.arj", "arj/stored.arj"),
    ("arj/method1.arj", "arj/method1.arj"),
    ("arj/method4.arj", "arj/method4.arj"),
    ("ace/license1.ace", "ace/license1.ace"),
    ("zoo/default.zoo", "zoo/default.zoo"),
    ("zoo/store.zoo", "zoo/store.zoo"),
    ("ha/copy.ha", "ha/copy.ha"),
    ("ha/hsc.ha", "ha/hsc.ha"),
    ("hyp/license.hyp", "hyp/license.hyp"),
    ("hyp/stored.hyp", "hyp/stored.hyp"),
    ("ice/license_lha.ice", "ice/license_lha.ice"),
    ("sq/license.sq", "qqq/license.sq"),
    ("sq/license.sq2", "qqq/license.sq2"),
    ("sqz/store.sqz", "sqz/store.sqz"),
    ("sqz/license_m1.sqz", "sqz/license_m1.sqz"),
    ("uc2/normal.uc2", "uc2/normal.uc2"),
    ("uc2/tight.uc2", "uc2/tight.uc2"),
    ("ue2/license.ue2", "ue2/license.ue2"),
    ("tar/license.tar", "tar/license.tar"),
    ("gz/LICENSE.gz", "gz/LICENSE.gz"),
    ("bz2/LICENSE.bz2", "bz2/LICENSE.bz2"),
    ("Z/LICENSE.Z", "Z/LICENSE.Z"),
    ("tgz/license.tgz", "tgz/license.tgz"),
    ("tbz/license.tar.bz2", "tbz/license.tar.bz2"),
    ("tarz/license.tar.Z", "tarz/license.tar.Z"),
    ("jar/license_m1.j", "jar/license_m1.j"),
    ("jar/license_m2.j", "jar/license_m2.j"),
    ("jar/license_m3.j", "jar/license_m3.j"),
    ("jar/license_m4.j", "jar/license_m4.j"),
    # The one real-world RAR on this machine: WinRAR 5 output, 28 entries. unarc-rs proves it
    # readable in tests/rar_decompression.rs:130-148; the same test is the oracle here.
    ("rar/acid-70a.rar", "acid-70a.rar"),
    # Deliberately corrupt archives, already proven to trip their own readers by unarc-rs's
    # failure tests. They belong to the malformed corpus, not the gate corpus.
    ("corrupt/wrongcrc16.arc", "arc/wrongcrc16.arc"),
    ("corrupt/wrongcrc32.arj", "arj/wrongcrc32.arj"),
    ("corrupt/wrongcrc16.zoo", "zoo/wrongcrc16.zoo"),
    ("corrupt/wrongcrc.sqz", "sqz/wrongcrc.sqz"),
    ("corrupt/invalid.hyp", "hyp/invalid.hyp"),
]

# Every vendored record: format, oracles, status. The long tail is pending on the legacy team;
# the expectation is today's honest Unsupported error, and the oracle proves the fixture valid.
LEGACY_PENDING = {
    "arc": ("arc", ["unarc"], "SECRET" if False else None),
    "arj": ("arj", ["unarc"], None),
    "ace": ("ace", ["unarc"], None),
    "zoo": ("zoo", ["unarc"], None),
    "ha": ("ha", ["unarc"], None),
    "hyp": ("hyp", ["unarc"], None),
    "ice": ("ice", ["unarc"], None),
    "sq": ("sq", ["unarc"], None),
    "sqz": ("sq", ["unarc"], None),  # unarc folds SQZ into its Sq reader
    "uc2": ("uc2", ["unarc"], None),
    "ue2": ("uc2", ["unarc"], None),
    "rar": ("rar", ["unarc"], None),
    "jar": ("zip", ["unarc"], None),
}


def find_unarc_tests():
    """The unpacked unarc-rs 0.7.2 sources, where the harvest comes from."""
    registry = Path.home() / ".cargo/registry/src"
    if not registry.is_dir():
        return None
    for candidate in sorted(registry.glob("*/unarc-rs-0.7.2/tests")):
        return candidate
    return None


def harvest_vendor():
    """Copies the curated unarc-rs fixtures into vendor/, recording provenance."""
    unarc = find_unarc_tests()
    origin_notes = []
    for dest, src in HARVEST:
        target = VENDOR / "unarc-rs-0.7.2" / dest
        target.parent.mkdir(parents=True, exist_ok=True)
        if unarc is not None:
            source = unarc / src
            if not source.is_file():
                print(f"harvest: missing source {source}", file=sys.stderr)
                sys.exit(1)
            shutil.copyfile(source, target)
        elif not target.is_file():
            print(
                f"harvest: unarc-rs sources not found and {target} is not checked in; "
                "the corpus cannot be regenerated here",
                file=sys.stderr,
            )
            sys.exit(1)
        origin_notes.append((dest, src, sha256(target)))
    return origin_notes


# ---------------------------------------------------------------------------
# Generated archives, one function per family.
# ---------------------------------------------------------------------------

def gen_zip(tmp: Path):
    stage(tmp / "tree")
    tree = tmp / "tree"

    def zip_path(name):
        return f"generated/zip/{name}"

    # store / deflate / bzip2 / lzma, via python's zipfile.
    made = {}
    for name, compress, label in [
        ("store.zip", zipfile.ZIP_STORED, "store"),
        ("deflate.zip", zipfile.ZIP_DEFLATED, "deflate"),
        ("bzip2.zip", zipfile.ZIP_BZIP2, "bzip2"),
        ("lzma.zip", zipfile.ZIP_LZMA, "lzma"),
    ]:
        out = GENERATED / "zip" / name
        with zipfile.ZipFile(out, "w", compress) as zf:
            for entry, content in PLAINTEXT:
                zf.writestr(zinfo(entry), content())
        made[name] = f"python3 zipfile, compress={label}"
        add_file(zip_path(name), "zip", made[name], entries=entries_of(), oracles=["unarc", "oxiarc"])

    # Data descriptor (GPBF bit 3): written through a non-seekable sink, which forces zipfile
    # to emit the descriptor. Verified: flag_bits == 0x8.
    class NonSeekable:
        def __init__(self):
            self.buf = bytearray()
        def write(self, b):
            self.buf.extend(b)
            return len(b)
        def flush(self):
            pass
    sink = NonSeekable()
    with zipfile.ZipFile(sink, "w", zipfile.ZIP_DEFLATED) as zf:
        for entry, content in PLAINTEXT:
            zf.writestr(zinfo(entry), content())
    (GENERATED / "zip" / "data-descriptor.zip").write_bytes(sink.buf)
    add_file(zip_path("data-descriptor.zip"), "zip",
             "python3 zipfile over a non-seekable sink (forces GPBF bit 3)",
             entries=entries_of(), oracles=["unarc", "oxiarc"])

    # UTF-8 names (GPBF bit 11).
    utf8_names = [("héllo wörld.txt", b"umlauts\n"), ("日本語.txt", b"nihongo\n")]
    with zipfile.ZipFile(GENERATED / "zip" / "utf8-names.zip", "w", zipfile.ZIP_DEFLATED) as zf:
        for name, content in utf8_names:
            zf.writestr(zinfo(name), content)
    add_file(zip_path("utf8-names.zip"), "zip", "python3 zipfile, non-ASCII names (GPBF bit 11)",
             entries=[{"name": n, "size": len(c), "crc32": zlib.crc32(c) & 0xFFFFFFFF, "dir": False}
                      for n, c in utf8_names],
             oracles=["unarc", "oxiarc"])

    # CP437 names: hand-built local + central headers, bit 11 clear. Neither python zipfile nor
    # Info-ZIP can produce this (both always set bit 11 for non-ASCII), and it is the only way to
    # exercise the CP437 decode tables end to end.
    name = "café.txt".encode("cp437")
    body = b"cp437 encoded name\n"
    crc = zlib.crc32(body) & 0xFFFFFFFF
    local = struct.pack("<IHHHHHIIIHH", 0x04034B50, 20, 0, 0, 0, 0, crc, len(body), len(body),
                        len(name), 0) + name + body
    central = struct.pack("<IHHHHHHIIIHHHHHII", 0x02014B50, 20, 20, 0, 0, 0, 0, crc, len(body),
                          len(body), len(name), 0, 0, 0, 0, 0, 0) + name
    eocd = struct.pack("<IHHHHIIH", 0x06054B50, 0, 0, 1, 1, len(central), len(local), 0)
    (GENERATED / "zip" / "cp437-names.zip").write_bytes(local + central + eocd)
    add_file(zip_path("cp437-names.zip"), "zip",
             "hand-built local+central headers, name encoded CP437, bit 11 clear",
             entries=[{"name": "café.txt", "size": len(body), "crc32": crc, "dir": False}],
             oracles=["unarc", "oxiarc"],
             notes="CP437 is the historic ZIP encoding; python and Info-ZIP cannot write it.")

    # Empty archive: EOCD only.
    (GENERATED / "zip" / "empty.zip").write_bytes(struct.pack("<IHHHHIIH", 0x06054B50, 0, 0, 0, 0, 0, 0, 0))
    add_file(zip_path("empty.zip"), "zip", "hand-written EOCD only (22 bytes)",
             entries=[], oracles=["unarc", "oxiarc"])

    # Minimal ZIP64: one stored entry whose sizes are 0xFFFFFFFF in the classic fields, with the
    # real sizes in the ZIP64 extra field, and a ZIP64 EOCD + locator before the classic EOCD.
    # python's zipfile only emits ZIP64 at >65535 entries or >4 GiB, neither of which belongs in
    # a test corpus.
    body = b"zip64 stored payload\n"
    crc = zlib.crc32(body) & 0xFFFFFFFF
    zname = b"z64.txt"
    extra64 = struct.pack("<HHQQ", 0x0001, 16, len(body), len(body))
    local = struct.pack("<IHHHHHIIIHH", 0x04034B50, 45, 0, 0, 0, 0, crc, 0xFFFFFFFF, 0xFFFFFFFF,
                        len(zname), len(extra64)) + zname + extra64 + body
    central = struct.pack("<IHHHHHHIIIHHHHHII", 0x02014B50, 45, 45, 0, 0, 0, 0, crc, 0xFFFFFFFF,
                          0xFFFFFFFF, len(zname), len(extra64), 0, 0, 0, 0, 0) + zname + extra64
    zip64_eocd = struct.pack("<IQHHIIQQQQ", 0x06064B50, 44, 45, 45, 0, 0, 1, 1,
                             len(central), len(local))
    zip64_locator = struct.pack("<IIQI", 0x07064B50, 0, 0, 1)
    eocd = struct.pack("<IHHHHIIH", 0x06054B50, 45, 45, 0xFFFF, 0xFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0)
    (GENERATED / "zip" / "zip64.zip").write_bytes(local + central + zip64_eocd + zip64_locator + eocd)
    add_file(zip_path("zip64.zip"), "zip",
             "hand-built minimal ZIP64 (0xFFFFFFFF classic sizes, ZIP64 extra + EOCD)",
             entries=[{"name": "z64.txt", "size": len(body), "crc32": crc, "dir": False}],
             oracles=["unarc", "oxiarc"])

    # Symlink, via Info-ZIP's -y.
    link_tree = tmp / "linktree"
    link_tree.mkdir(parents=True)
    (link_tree / "hello.txt").write_bytes(hello())
    os.utime(link_tree / "hello.txt", (FIXED_MTIME, FIXED_MTIME))
    (link_tree / "link.txt").symlink_to("hello.txt")
    os.utime(link_tree / "link.txt", (FIXED_MTIME, FIXED_MTIME), follow_symlinks=False)
    # -X drops Info-ZIP's extended-timestamp extra field, which carries atime and ctime and
    # would make the archive change every run.
    subprocess.run(["zip", "-q", "-y", "-X", str(GENERATED / "zip" / "symlink.zip"),
                    "hello.txt", "link.txt"],
                   cwd=link_tree, check=True)
    add_file(zip_path("symlink.zip"), "zip", "zip -y over a symlink tree",
             entries=None, oracles=[],
             notes="Symlink semantics differ per oracle, so this record is checked by a bespoke "
                   "test in differential.rs (kind Symlink, target hello.txt), not the generic "
                   "comparison.")

    # ZipCrypto encryption, via zip -P. The password check bytes carry a random salt, so this
    # archive is not byte-reproducible; its entry plaintext is.
    subprocess.run(["zip", "-q", "-P", "secret", str(GENERATED / "zip" / "zipcrypto.zip")] +
                   [e[0] for e in PLAINTEXT],
                   cwd=tree, check=True)
    add_file(zip_path("zipcrypto.zip"), "zip", "zip -P secret (traditional PKWARE encryption)",
             password="secret", entries=entries_of(), oracles=["unarc"],
             notes="Listing works and marks the entries; reading needs the password. Encrypted "
                   "ZIP is currently refused by this crate — the dedicated test in "
                   "differential.rs pins Err(Password) without a password; with one, this "
                   "record flips to gate once the ZipCrypto reader lands.")


def gen_7z(tmp: Path):
    stage(tmp / "tree")
    tree = tmp / "tree"
    files = [e[0] for e in PLAINTEXT]

    def sevenz(name, *extra):
        out = GENERATED / "7z" / name
        run(["7z", "a", "-t7z", "-bso0", "-bsp0", str(out), *extra, *files], cwd=tree)
        return f"7z a -t7z {' '.join(extra)} {name}  (over hello.txt data.bin empty.txt nested/inner.txt)"

    solid = sevenz("solid.7z", "-ms=on")
    add_file("generated/7z/solid.7z", "7z", solid, entries=entries_of(), oracles=["sevenz", "unarc"],
             notes="Solid: every entry in one folder. The differential exercises in-order and "
                   "out-of-order reads against the same bytes.")
    non_solid = sevenz("non-solid.7z", "-ms=off")
    add_file("generated/7z/non-solid.7z", "7z", non_solid, entries=entries_of(),
             oracles=["sevenz", "unarc"])
    lzma = sevenz("lzma.7z", "-m0=lzma2")
    add_file("generated/7z/lzma.7z", "7z", lzma, entries=entries_of(), oracles=["sevenz", "unarc"])

    enc = run(["7z", "a", "-t7z", "-bso0", "-bsp0", "-psecret",
               str(GENERATED / "7z" / "aes256.7z"), *files], cwd=tree)
    add_file("generated/7z/aes256.7z", "7z", enc, password="secret", entries=entries_of(),
             oracles=["sevenz"], notes="AES-256, data encrypted only; listing works without the "
                                       "password. Random salt: not byte-reproducible.")
    enc_h = run(["7z", "a", "-t7z", "-bso0", "-bsp0", "-psecret", "-mhe=on",
                 str(GENERATED / "7z" / "aes256-header.7z"), *files], cwd=tree)
    add_file("generated/7z/aes256-header.7z", "7z", enc_h, password="secret", entries=entries_of(),
             oracles=["sevenz"], notes="AES-256 with header encryption: opening itself needs the "
                                       "password. Random salt: not byte-reproducible.")

    # The 400-entry solid pair behind the 15.5 s vs 204 ms measurement in lib.rs — correctness
    # input today, benchmark input for whoever writes the solid benchmark.
    for solid_on, name in [(True, "solid400.7z"), (False, "non-solid400.7z")]:
        big = tmp / "big"
        big.mkdir(exist_ok=True)
        expected = []
        for i in range(400):
            content = bytes(((i + j) * 17) % 256 for j in range(200))
            path = big / f"f{i:03}.bin"
            path.write_bytes(content)
            os.utime(path, (FIXED_MTIME, FIXED_MTIME))
            expected.append({"name": f"f{i:03}.bin", "size": len(content),
                             "crc32": zlib.crc32(content) & 0xFFFFFFFF, "dir": False})
        flag = "-ms=on" if solid_on else "-ms=off"
        run(["7z", "a", "-t7z", "-bso0", "-bsp0", flag, str(GENERATED / "7z" / name), "."], cwd=big)
        add_file(f"generated/7z/{name}", "7z", f"7z a -t7z {flag} over 400 generated files",
                 entries=expected, oracles=["sevenz"],
                 notes="The shape of the quadratic-cost measurement in lib.rs:39-41.")


def gen_tar(tmp: Path):
    stage(tmp / "tree")
    tree = tmp / "tree"
    listing = tmp / "list.txt"
    listing.write_text("\n".join(e[0] for e in PLAINTEXT) + "\n")

    for fmt, name in [("ustar", "ustar.tar"), ("gnu", "gnu.tar"), ("posix", "posix.tar")]:
        out = GENERATED / "tar" / name
        # posix/pax records atime and ctime in an extended header, and the kernel bumps atime
        # while tar reads the file, so without the delete options the archive changes every run.
        extra = ["--pax-option=delete=atime,delete=ctime"] if fmt == "posix" else []
        run(["tar", f"--format={fmt}", *extra, "-cf", str(out), "-T", str(listing)], cwd=tree)
        add_file(f"generated/tar/{name}", "tar",
                 f"tar --format={fmt} {' '.join(extra)} -cf {name} -T list".replace("  ", " "),
                 entries=entries_of(), oracles=["unarc", "oxiarc"])

    # A tar with the awkward cases: a directory, a symlink, an executable, a >100-char name.
    awkward = tmp / "awkward.tar"
    with tarfile.open(awkward, "w", format=tarfile.GNU_FORMAT) as tf:
        info = tarfile.TarInfo("dir")
        info.type = tarfile.DIRTYPE
        info.mode = 0o755
        info.mtime = FIXED_MTIME
        tf.addfile(info)
        info = tarfile.TarInfo("exec.sh")
        info.size = len(hello())
        info.mode = 0o755
        info.mtime = FIXED_MTIME
        tf.addfile(info, fileobj=__import__("io").BytesIO(hello()))
        info = tarfile.TarInfo("link.txt")
        info.type = tarfile.SYMTYPE
        info.linkname = "exec.sh"
        info.mtime = FIXED_MTIME
        tf.addfile(info)
        long_name = "a" * 120 + ".txt"
        info = tarfile.TarInfo(long_name)
        info.size = len(inner())
        info.mtime = FIXED_MTIME
        tf.addfile(info, fileobj=__import__("io").BytesIO(inner()))
    shutil.copyfile(awkward, GENERATED / "tar" / "awkward.tar")
    add_file("generated/tar/awkward.tar", "tar",
             "python3 tarfile, GNU format: directory, symlink, executable, 121-char name",
             entries=[
                 {"name": "dir", "size": 0, "crc32": 0, "dir": True},
                 {"name": "exec.sh", "size": len(hello()), "crc32": zlib.crc32(hello()) & 0xFFFFFFFF, "dir": False},
                 {"name": "link.txt", "size": 0, "crc32": 0, "dir": False},
                 {"name": "a" * 120 + ".txt", "size": len(inner()), "crc32": zlib.crc32(inner()) & 0xFFFFFFFF, "dir": False},
             ],
             oracles=["unarc", "oxiarc"],
             notes="link.txt is a symlink: contents are not compared, kind and target are.")

    # Compressed tars. The tar is built once and piped through each compressor from python, so
    # gzip gets -n: the CLI's tar -czf embeds the tar's mtime in the gzip header.
    streams = [
        ("tar.gz", ["gzip", "-n", "-c"], "tar --format=ustar | gzip -n"),
        ("tar.bz2", ["bzip2", "-c"], "tar --format=ustar | bzip2"),
        ("tar.xz", ["xz", "-c"], "tar --format=ustar | xz"),
        ("tar.zst", ["zstd", "-q", "-c"], "tar --format=ustar | zstd -q"),
    ]
    tar_bytes = subprocess.run(
        ["tar", "--format=ustar", "-cf", "-", "-T", str(listing)],
        cwd=tree, check=True, capture_output=True,
    ).stdout
    for name, argv, label in streams:
        out = GENERATED / "streams" / f"plain.{name}"  # plain.tar.gz, plain.tar.bz2, …
        out.write_bytes(subprocess.run(argv, input=tar_bytes, check=True, capture_output=True).stdout)
        add_file(f"generated/streams/plain.{name}", name,
                 f"{label} over the standard plaintext tree",
                 entries=entries_of(), oracles=["unarc"])


def gen_streams(tmp: Path):
    stage(tmp / "tree")
    tree = tmp / "tree"
    payload = data_bin()
    payload_crc = zlib.crc32(payload) & 0xFFFFFFFF
    single = [{"name": "content", "size": len(payload), "crc32": payload_crc, "dir": False}]

    for codec, argv, label in [
        ("gz", ["gzip", "-k", "-f", "-c"], "gzip -c data.bin"),
        ("bz2", ["bzip2", "-k", "-f", "-c"], "bzip2 -c data.bin"),
        ("xz", ["xz", "-k", "-f", "-c"], "xz -c data.bin"),
        ("zst", ["zstd", "-q", "-f", "-c"], "zstd -q -c data.bin"),
    ]:
        out = GENERATED / "streams" / f"payload.{codec}"
        with open(out, "wb") as fh:
            subprocess.run(argv + ["data.bin"], cwd=tree, stdout=fh, check=True)
        add_file(f"generated/streams/payload.{codec}", codec, label,
                 entries=single, oracles=(["oxiarc"] if codec != "zst" else ["oxiarc"]))

    # Multi-member gzip: two gzip members concatenated; the decompressed stream is the
    # concatenation of both plaintexts. This is the case MultiGzDecoder exists for.
    part1, part2 = b"member one\n", b"member two\n"
    m1 = subprocess.run(["gzip", "-c"], input=part1, check=True, capture_output=True).stdout
    m2 = subprocess.run(["gzip", "-c"], input=part2, check=True, capture_output=True).stdout
    (GENERATED / "streams" / "multimember.gz").write_bytes(m1 + m2)
    joined = part1 + part2
    add_file("generated/streams/multimember.gz", "gz",
             "two gzip members concatenated (member one + member two)",
             entries=[{"name": "content", "size": len(joined),
                       "crc32": zlib.crc32(joined) & 0xFFFFFFFF, "dir": False}],
             oracles=[],
             notes="oxiarc's gzip decompressor reads only the first member, so the generic "
                   "oracle comparison would be wrong here; the manifest alone owns this record.")

    # The multi-member case as a compressed TAR: gzip(ustar.tar) via the CLI, plus the
    # concatenation of two separately-gzipped tars is not a thing any tool writes — skip.
    gz_tar = subprocess.run(["gzip", "-c", "tar/ustar.tar"], cwd=GENERATED,
                            check=True, capture_output=True).stdout
    (GENERATED / "streams" / "plain-from-gzip.tar.gz").write_bytes(gz_tar)
    # Keep only one tar.gz: drop this duplicate, the CLI one in gen_tar is the record.
    (GENERATED / "streams" / "plain-from-gzip.tar.gz").unlink()


def gen_rust_fragment():
    """Runs examples/gen_corpus.rs, which writes the codecs no CLI here can do + a JSON fragment."""
    env = dict(os.environ)
    run(["cargo", "run", "-q", "-p", "libarchive", "--offline", "--example", "gen_corpus"],
        cwd=REPO, env=env)
    fragment = json.loads((GENERATED / "rust-manifest-fragment.json").read_text())
    for record in fragment["files"]:
        FILES.append(record)


# ---------------------------------------------------------------------------
# Malformed corpus: truncated, wrong-magic, oversized, corrupt, traversal.
# ---------------------------------------------------------------------------

# One small gate file per format to mutate. (path under corpus root, format string)
MUTATION_SOURCES = [
    ("generated/zip/store.zip", "zip"),
    ("generated/7z/lzma.7z", "7z"),
    ("generated/tar/ustar.tar", "tar"),
    ("generated/streams/plain.tar.gz", "tar.gz"),
    ("generated/streams/plain.tar.bz2", "tar.bz2"),
    ("generated/streams/plain.tar.xz", "tar.xz"),
    ("generated/streams/plain.tar.zst", "tar.zst"),
    ("generated/streams/payload.gz", "gz"),
    ("generated/streams/payload.bz2", "bz2"),
    ("generated/streams/payload.xz", "xz"),
    ("generated/streams/payload.zst", "zst"),
    ("vendor/unarc-rs-0.7.2/rar/acid-70a.rar", "rar"),
    ("vendor/unarc-rs-0.7.2/arc/store.arc", "arc"),
    ("vendor/unarc-rs-0.7.2/arj/stored.arj", "arj"),
    ("vendor/unarc-rs-0.7.2/ace/license1.ace", "ace"),
    ("vendor/unarc-rs-0.7.2/zoo/default.zoo", "zoo"),
    ("vendor/unarc-rs-0.7.2/ha/copy.ha", "ha"),
    ("vendor/unarc-rs-0.7.2/hyp/license.hyp", "hyp"),
    ("vendor/unarc-rs-0.7.2/ice/license_lha.ice", "ice"),
    ("vendor/unarc-rs-0.7.2/sq/license.sq", "sq"),
    ("vendor/unarc-rs-0.7.2/sqz/store.sqz", "sq"),
    ("vendor/unarc-rs-0.7.2/uc2/normal.uc2", "uc2"),
    ("vendor/unarc-rs-0.7.2/ue2/license.ue2", "uc2"),
    ("vendor/unarc-rs-0.7.2/jar/license_m1.j", "zip"),
    ("vendor/unarc-rs-0.7.2/Z/LICENSE.Z", "z"),
]


def gen_malformed(tmp: Path):
    out_dir = GENERATED / "malformed"
    out_dir.mkdir(parents=True, exist_ok=True)

    for rel, format_ in MUTATION_SOURCES:
        source = CORPUS / rel
        if not source.is_file():
            print(f"malformed: source {source} missing", file=sys.stderr)
            sys.exit(1)
        raw = source.read_bytes()
        stem = rel.replace("/", "_").replace(".", "_")
        # Truncation at lengths that are actually shorter than the file: a "truncation" that
        # keeps every byte is a valid archive and would poison the corpus. Empty and one-byte
        # files must Err at open; a mid-file cut may legitimately still list entry 0, so its
        # contract is no-panic, plus an Err when the reader gets as far as the missing bytes.
        cuts = [(0, "err"), (1, "err")]
        if len(raw) > 512:
            cuts.append((512, "err"))
        if len(raw) > 2:
            cuts.append((len(raw) // 2, "open_no_panic"))
        cuts.append((len(raw) - 1, "open_no_panic"))
        for cut, expect in cuts:
            target = out_dir / f"trunc-{cut}-{stem}"
            target.write_bytes(raw[:cut])
            add_malformed(f"generated/malformed/{target.name}", "truncated", format_,
                          expect=expect,
                          notes=f"{rel} truncated to {cut} of {len(raw)} bytes")
        # Wrong magic: flip the first byte. Detection then cannot see the real format; the
        # fallback is ZIP, which must then report NotAnArchive/Malformed — an Err, never a panic
        # and never a hang.
        flipped = bytearray(raw)
        if flipped:
            flipped[0] ^= 0xFF
        target = out_dir / f"wrongmagic-{stem}"
        target.write_bytes(bytes(flipped))
        add_malformed(f"generated/malformed/{target.name}", "wrong_magic", format_,
                      expect="err",
                      notes=f"{rel} with its first byte XOR 0xFF")

    # Deliberately corrupt archives that ship with their own failure stories.
    for rel, format_, notes in [
        ("vendor/unarc-rs-0.7.2/corrupt/wrongcrc16.arc", "arc", "unarc-rs arc_failures.rs: this must fail a CRC check, not pass"),
        ("vendor/unarc-rs-0.7.2/corrupt/wrongcrc32.arj", "arj", "unarc-rs arj_failures.rs"),
        ("vendor/unarc-rs-0.7.2/corrupt/wrongcrc16.zoo", "zoo", "unarc-rs zoo_failures.rs"),
        ("vendor/unarc-rs-0.7.2/corrupt/wrongcrc.sqz", "sq", "unarc-rs sqz_failures.rs"),
        ("vendor/unarc-rs-0.7.2/corrupt/invalid.hyp", "hyp", "26 bytes; unarc-rs hyp_failures.rs"),
        ("../vendor/sevenz-rust2/tests/resources/issue_127_coder_stream_overflow.bin", "7z",
         "sevenz-rust2's regression input for issue #127 (coder stream overflow)"),
    ]:
        # These live in vendor/, not generated/; the record still points at them. The issue_127
        # bin is the one record here whose behaviour is unknown — upstream fixed an *overflow*,
        # not a rejection, so the contract is no-panic, not Err.
        expect = "open_no_panic" if "issue_127" in rel else "err"
        add_malformed(rel, "corrupt", format_, expect=expect, notes=notes)

    # Oversized headers: a TAR entry claiming ~8 GiB, a ZIP entry claiming 4 GiB-2 without a
    # ZIP64 extra, and a gzip bomb under a 1 MiB ceiling. Each must be refused with an Err
    # (LimitExceeded or Malformed) before anything is allocated.
    header = bytearray(512)
    header[0:9] = b"oversized"
    header[100:108] = b"0000644\0"
    header[108:116] = b"0000000\0"
    header[116:124] = b"0000000\0"
    header[124:136] = b"777777777777"  # size: the largest an octal field can hold
    header[136:148] = b"00000000000\0"  # mtime
    header[156] = ord("0")              # regular file
    header[257:263] = b"ustar\0"
    header[263:265] = b"00"
    checksum = sum(header) - sum(header[148:156]) + (8 * 32)
    header[148:156] = f"{checksum:06o}\0 ".encode()
    (out_dir / "oversized-tar.tar").write_bytes(bytes(header) + b"\0" * 512)
    add_malformed("generated/malformed/oversized-tar.tar", "oversized", "tar",
                  expect="err", error_kind="LimitExceeded",
                  notes="one ustar header claiming 8589934591 bytes; must be refused by the "
                        "per-entry ceiling before any allocation")

    # ZIP: stored entry whose uncompressed size is 0xFFFFFFFE with no ZIP64 extra.
    zname = b"big.bin"
    body = b"x" * 16  # the header lies about the size; only 16 bytes follow
    crc = zlib.crc32(body) & 0xFFFFFFFF
    local = struct.pack("<IHHHHHIIIHH", 0x04034B50, 20, 0, 0, 0, 0, crc, len(body), 0xFFFFFFFE,
                        len(zname), 0) + zname + body
    central = struct.pack("<IHHHHHHIIIHHHHHII", 0x02014B50, 20, 20, 0, 0, 0, 0, crc, len(body),
                          0xFFFFFFFE, len(zname), 0, 0, 0, 0, 0, 0) + zname
    eocd = struct.pack("<IHHHHIIH", 0x06054B50, 0, 0, 1, 1, len(central), len(local), 0)
    (out_dir / "oversized-zip.zip").write_bytes(local + central + eocd)
    add_malformed("generated/malformed/oversized-zip.zip", "oversized", "zip",
                  expect="err",
                  notes="central directory claims 0xFFFFFFFE uncompressed bytes with no ZIP64 "
                        "extra field; must be Malformed, not a 4 GiB allocation")

    # A gzip bomb: 4 MiB of zeros compresses to a few KiB. Under a 1 MiB entry ceiling the read
    # must come back LimitExceeded, and quickly.
    bomb = subprocess.run(["gzip", "-c"], input=b"\0" * (4 * 1024 * 1024), check=True,
                          capture_output=True).stdout
    (out_dir / "bomb.gz").write_bytes(bomb)
    add_malformed("generated/malformed/bomb.gz", "oversized", "gz",
                  expect="err", error_kind="LimitExceeded",
                  notes="4 MiB of zeros, ~4 KiB gzipped; the malformed suite reads it under a "
                        "1 MiB ceiling (support::tight_limits), never the 4 GiB default")

    # Path traversal: valid archives whose names escape the extraction root. These must OPEN —
    # the bytes are well-formed, and a reader that refuses them would hide the entries from the
    # file manager's listing. Sanitising is the extraction layer's job (bridge contract,
    # J-0005; coord/notes/testing-plan.md §4). The assertion is no panic, and the normalised
    # names round-trip as data.
    traversal_names = ["../../etc/passwd", "/absolute/escape.txt", "..\\..\\windows\\evil.dll"]
    traversal_body = b" traversal payload \n"
    with zipfile.ZipFile(out_dir / "traversal.zip", "w", zipfile.ZIP_STORED) as zf:
        for name in traversal_names:
            zf.writestr(zinfo(name), traversal_body)
    add_malformed("generated/malformed/traversal.zip", "traversal", "zip", expect="open_no_panic",
                  notes="well-formed ZIP with ../../, absolute and backslash names; must list "
                        "and stream, with names normalised, never panic")
    with tarfile.open(out_dir / "traversal.tar", "w") as tf:
        import io
        for name in traversal_names:
            info = tarfile.TarInfo(name)
            info.size = len(traversal_body)
            info.mtime = FIXED_MTIME
            tf.addfile(info, io.BytesIO(traversal_body))
    add_malformed("generated/malformed/traversal.tar", "traversal", "tar", expect="open_no_panic",
                  notes="ditto, TAR flavour")

    # Wrong magic for the formats with no corpus file of their own: brotli (no magic — any
    # bytes fall through detection to ZIP) and UCB (magic present, reader absent).
    (out_dir / "garbage.br").write_bytes(bytes(range(256)) * 4)
    add_malformed("generated/malformed/garbage.br", "wrong_magic", "br", expect="err",
                  notes="brotli has no signature; detection falls through to ZIP, which must "
                        "then report NotAnArchive/Malformed")
    (out_dir / "fake.ucb").write_bytes(b"UCh\x1a\x00" + b"\xff" * 64)
    add_malformed("generated/malformed/fake.ucb", "wrong_magic", "ucb", expect="err",
                  notes="recognisable UCB magic over garbage: detection succeeds, the reader "
                        "must Err (Unsupported today) rather than parse")
    # And the same for every remaining legacy format, magic-only over garbage: detection must
    # recognise the signature and the open must Err cleanly. (ARC has no fixed magic — its
    # "magic" is a header CRC — so it is covered by the truncations above rather than a fake.)
    for magic, format_, name in [
        (b"Rar!\x1a\x07\x00" + b"\xff" * 32, "rar", "fake-rar.rar"),
        (b"**ACE**" + b"\xff" * 32, "ace", "fake-ace.ace"),
        (b"ZOO 200" + b"\xff" * 32, "zoo", "fake-zoo.zoo"),
        (b"\x60\xea" + b"\xff" * 64, "arj", "fake-arj.arj"),
        (b"SQ\x1a" + b"\xff" * 64, "sq", "fake-sq.sq"),
        (b"\x1f\xa0" + b"\xff" * 64, "ice", "fake-ice.ice"),
        (b"ZZX\x00" + b"\xff" * 64, "hyp", "fake-hyp.hyp"),
        (b"\x19\x01HA" + b"\xff" * 64, "ha", "fake-ha.ha"),
    ]:
        (out_dir / name).write_bytes(magic)
        add_malformed(f"generated/malformed/{name}", "wrong_magic", format_, expect="err",
                      notes="magic bytes over garbage: detection must recognise, open must Err")
    # LZH needs its magic *and* its method byte (offset 20) right for detection to pick it up.
    lzh = bytearray(b"\xff" * 90)
    lzh[0:5] = b"-lh5-"
    lzh[20] = ord("l")
    (out_dir / "fake-lzh.lzh").write_bytes(bytes(lzh))
    add_malformed("generated/malformed/fake-lzh.lzh", "wrong_magic", "lzh", expect="err",
                  notes="valid LZH magic + method byte over garbage: detection must recognise "
                        "the format, open must Err")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    # Start from nothing: a stale file left by an interrupted run would be picked up by the
    # mutation pass and committed as if the manifest knew about it.
    for stale in (GENERATED, VENDOR):
        if stale.exists():
            shutil.rmtree(stale)
    for sub in ["zip", "7z", "tar", "streams", "malformed"]:
        (GENERATED / sub).mkdir(parents=True, exist_ok=True)
    VENDOR.mkdir(parents=True, exist_ok=True)

    harvest = harvest_vendor()
    add_vendor_manifest(harvest)

    with tempfile.TemporaryDirectory(dir="/tmp") as tmp_str:
        tmp = Path(tmp_str)
        gen_zip(tmp)
        gen_7z(tmp)
        gen_tar(tmp)
        gen_streams(tmp)
        gen_rust_fragment()
        gen_malformed(tmp)

    # Deterministic order, stable JSON.
    FILES.sort(key=lambda record: record["path"])
    MALFORMED.sort(key=lambda record: record["path"])
    manifest = {
        "comment": (
            "The corpus contract. Regenerate with: python3 libarchive/tests/corpus/generate.py. "
            "Entry ground truth comes from the plaintext, never from reading an archive back. "
            "status=gate records must pass the differential harness; status=pending records "
            "assert today's honest behaviour and name the owner who flips them when their reader "
            "lands. Unencrypted archives regenerate byte-for-byte; encrypted archives (random "
            "salt) and the LZH fixtures (writer timestamps) do not."
        ),
        "files": FILES,
        "malformed": MALFORMED,
    }
    MANIFEST.write_text(json.dumps(manifest, indent=1, sort_keys=False, ensure_ascii=False) + "\n")
    print(f"wrote {len(FILES)} file records and {len(MALFORMED)} malformed records")


def add_vendor_manifest(harvest):
    """The vendored records: long tail pending on the legacy team, streams gate."""
    hashes = {dest: digest for dest, _src, digest in harvest}
    notes_by_path = {}

    def vend(dest, format_, status, owner, password, oracles, notes, expect_kind="ok", error_kind=None):
        add_file(f"vendor/unarc-rs-0.7.2/{dest}", format_,
                 f"harvested from unarc-rs 0.7.2 tests/{HARVEST_SRC.get(dest, dest)}, "
                 f"sha256={hashes.get(dest, '?')[:16]}…",
                 status=status, owner=owner, password=password, entries=None,
                 oracles=oracles, notes=notes, expect_kind=expect_kind, error_kind=error_kind)

    # destination -> original source path in unarc-rs
    global HARVEST_SRC
    HARVEST_SRC = {dest: src for dest, src in HARVEST}

    # The long tail. Every record is pending on team legacy; the expectation is the Err this
    # build produces today (Unsupported), and the unarc oracle proves the fixture is valid, so
    # a broken harvest cannot masquerade as a legacy gap.
    pending = [
        ("arc/store.arc", "arc", None),
        ("arc/crunch.arc", "arc", None),
        ("arc/license_cypted.arc", "arc", "SECRET"),
        ("arj/stored.arj", "arj", None),
        ("arj/method1.arj", "arj", None),
        ("arj/method4.arj", "arj", None),
        ("ace/license1.ace", "ace", None),
        ("zoo/default.zoo", "zoo", None),
        ("zoo/store.zoo", "zoo", None),
        ("ha/copy.ha", "ha", None),
        ("ha/hsc.ha", "ha", None),
        ("hyp/license.hyp", "hyp", None),
        ("hyp/stored.hyp", "hyp", None),
        ("ice/license_lha.ice", "ice", None),
        ("sq/license.sq", "sq", None),
        ("sq/license.sq2", "sq", None),
        ("sqz/store.sqz", "sq", None),
        ("sqz/license_m1.sqz", "sq", None),
        ("uc2/normal.uc2", "uc2", None),
        ("uc2/tight.uc2", "uc2", None),
        ("ue2/license.ue2", "uc2", None),
        ("rar/acid-70a.rar", "rar", None),
    ]
    for dest, format_, password in pending:
        vend(dest, format_, "pending", "legacy", password, ["unarc"],
             "long-tail reader is formats::legacy::open, a stub; flip to gate when it lands",
             expect_kind="error", error_kind="Unsupported")

    # JAR: a ZIP with the four JAR compression methods (1-4). Listing must work today (the
    # central directory is ordinary ZIP); reading a method-1..4 entry must Err rather than
    # produce wrong bytes.
    for method in 1, 2, 3, 4:
        vend(f"jar/license_m{method}.j", "zip", "pending", "codecs", None, ["unarc"],
             f"JAR method {method}: listing works, reading must Err (Unsupported) until the "
             "method lands; unarc's jar reader decodes it",
             expect_kind="error", error_kind=None)

    # Streams and TARs the app ships today: these gate.
    vend("tar/license.tar", "tar", "gate", None, None, ["unarc", "oxiarc"],
         "unarc-rs's own TAR fixture")
    vend("gz/LICENSE.gz", "gz", "gate", None, None, ["unarc", "oxiarc"],
         "unarc-rs's own gzip fixture; the plaintext is the repo LICENSE, 11357 bytes")
    vend("bz2/LICENSE.bz2", "bz2", "gate", None, None, ["unarc", "oxiarc"],
         "unarc-rs's own bzip2 fixture")
    vend("Z/LICENSE.Z", "z", "pending", "codecs", None, ["unarc"],
         "Unix compress (.Z): detection works, listing works, decompression is "
         "Unimplemented — `compress` is LZW with a twist and has no crate here "
         "(src/compression.rs:71). Flip to gate when a Z codec lands.",
         expect_kind="error", error_kind="Unimplemented")
    vend("tgz/license.tgz", "tar.gz", "gate", None, None, ["unarc"],
         "tar.gz; unarc's Tgz reader is the oracle")
    vend("tbz/license.tar.bz2", "tar.bz2", "gate", None, None, ["unarc"],
         "tar.bz2; unarc's Tbz reader is the oracle")
    vend("tarz/license.tar.Z", "z", "pending", "codecs", None, ["unarc"],
         "tar.Z: a Unix-compress stream wrapping a TAR. Read at the stream level (format z); "
         "reading it as a compressed TAR needs the Z codec first.",
         expect_kind="error", error_kind="Unimplemented")


HARVEST_SRC = {}


if __name__ == "__main__":
    main()
