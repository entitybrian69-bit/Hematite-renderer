#!/usr/bin/env python3
"""Assemble and sign the Hematite Android APK without Gradle, aapt2, apksigner or a JDK.

Inputs it needs
---------------
* `target/aarch64-linux-android/release/libhematite_plugin.so`, cross-compiled by
  `./build.sh android` (which uses `tools/android_linker.py`).
* Python packages installed under `.cache/pylibs` by `./build.sh apk`: `cryptography`
  (RSA key, self-signed certificate, PKCS#7 signature).

What it produces
----------------
`dist/hematite-debug.apk` containing:
    AndroidManifest.xml        binary AXML encoded by this file
    assets/HEMATITE_STATUS.txt honest description of what the library does and does not do
    lib/arm64-v8a/libhematite.so
    META-INF/MANIFEST.MF, CERT.SF, CERT.RSA     APK Signature Scheme v1
    APK Signing Block with scheme v2 signatures

Why the encoding lives here
---------------------------
`aapt2`/`apksigner` are Java tools and no JDK is reachable in this environment, so the
binary Android XML format, the 16 KB-aligned ZIP layout and both signature schemes are
implemented directly. Every structure is verified afterwards by
`tools/verify_apk.py` (androguard parses the manifest, apksigtool verifies the signature
blocks, and the ELF parser in `tools/android_linker.py` inspects the packaged library).

The APK is a *container for the native library*. It has no Java code, no launcher
activity and no GL entry points; see docs/KNOWN_LIMITATIONS.md.
"""

from __future__ import annotations

import base64
import hashlib
import io
import os
import struct
import sys
import zipfile
import datetime
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from cryptography import x509  # noqa: E402
from cryptography.hazmat.primitives import hashes, serialization  # noqa: E402
from cryptography.hazmat.primitives.asymmetric import padding, rsa  # noqa: E402
from cryptography.hazmat.primitives.serialization import pkcs7  # noqa: E402
from cryptography.x509.oid import NameOID  # noqa: E402

# ---------------------------------------------------------------- AXML encoding
RES_XML = 0x0003
RES_STRING_POOL = 0x0001
RES_XML_RESOURCE_MAP = 0x0180
RES_XML_START_NAMESPACE = 0x0100
RES_XML_END_NAMESPACE = 0x0101
RES_XML_START_ELEMENT = 0x0102
RES_XML_END_ELEMENT = 0x0103
UTF8_FLAG = 1 << 8
NO_STRING = 0xFFFFFFFF
TYPE_STRING = 0x03
TYPE_INT_DEC = 0x10
TYPE_INT_HEX = 0x11
TYPE_INT_BOOLEAN = 0x12

ANDROID_NS = "http://schemas.android.com/apk/res/android"

# Attribute resource IDs from AOSP frameworks/base/core/res/res/values/public.xml.
# tools/verify_apk.py checks each of these against androguard's bundled copy of the
# same table, so a typo here fails verification instead of shipping a bad manifest.
ATTR_IDS = {
    "versionCode": 0x0101021B,
    "versionName": 0x0101021C,
    "minSdkVersion": 0x0101020C,
    "targetSdkVersion": 0x01010270,
    "label": 0x01010001,
    "hasCode": 0x0101000C,
    "debuggable": 0x0101000F,
    "extractNativeLibs": 0x010104EA,
    "allowBackup": 0x01010280,
    "name": 0x01010003,
    "value": 0x01010024,
    "resource": 0x01010025,
    "exported": 0x01010010,
}

# The FCL/Zalith renderer-plugin contract, as read by
# ZalithLauncher2 `game/plugin/renderer/RendererPluginManager.kt` (lines 88-145):
#   metaData.getBoolean("fclPlugin") || metaData.getBoolean("zalithRendererPlugin")
#   metaData.getString("renderer")  -> "<id>:<gl library>:<egl library>"
#   metaData.getString("des")       -> display name
#   metaData.getString("pojavEnv")  -> "KEY=VALUE:KEY=VALUE"
# Plugins are discovered by PluginLoader.kt:60-64 with
# queryIntentActivities(Intent(ACTION_MAIN)), so an <activity> with an ACTION_MAIN
# filter is mandatory even though the launcher never starts it. See
# docs/PLUGIN_CONTRACT.md for the full citation list.
PLUGIN_ID = "hematite"
NATIVE_LIBRARY = "libhematite.so"
PLUGIN_ACTIVITY = "org.hematite.renderer.PluginInfo"
PLUGIN_ENV = "POJAV_RENDERER=hematite:LIBGL_ES=2:LIBGL_MIPMAP=3:LIBGL_NOERROR=1"


@dataclass
class Attribute:
    """One manifest attribute: `android:`-namespaced unless `android` is False."""
    name: str
    value: object
    kind: str = "string"  # string | int | hex | bool
    android: bool = True


@dataclass
class Element:
    """One manifest element, with children nested in order."""
    name: str
    attributes: list[Attribute] = field(default_factory=list)
    children: list["Element"] = field(default_factory=list)


def encode_string_pool(strings: list[str]) -> bytes:
    """Encode a UTF-8 string pool chunk in the layout AssetManager expects."""
    offsets, data = [], bytearray()
    for text in strings:
        offsets.append(len(data))
        encoded = text.encode("utf-8")
        # UTF-8 pools store a character count then a byte count, each 1 or 2 bytes.
        for count in (len(text), len(encoded)):
            if count < 0x80:
                data.append(count)
            else:
                data.append(((count >> 8) & 0x7F) | 0x80)
                data.append(count & 0xFF)
        data += encoded
        data.append(0)
    while len(data) % 4:
        data.append(0)
    strings_start = 28 + 4 * len(strings)
    # ResStringPool_header is exactly 28 bytes: type, headerSize, size, stringCount,
    # styleCount, flags, stringsStart, stylesStart.
    header = struct.pack("<HHIIIIII", RES_STRING_POOL, 28, strings_start + len(data),
                         len(strings), 0, UTF8_FLAG, strings_start, 0)
    return header + b"".join(struct.pack("<I", o) for o in offsets) + bytes(data)


def typed_value(attribute: Attribute, index_of) -> tuple[int, int, int]:
    """Return (rawValue, dataType, data) for one attribute."""
    if attribute.kind == "string":
        return index_of(str(attribute.value)), TYPE_STRING, index_of(str(attribute.value))
    if attribute.kind == "bool":
        return NO_STRING, TYPE_INT_BOOLEAN, 0xFFFFFFFF if attribute.value else 0
    if attribute.kind == "hex":
        return NO_STRING, TYPE_INT_HEX, int(attribute.value) & 0xFFFFFFFF
    return NO_STRING, TYPE_INT_DEC, int(attribute.value) & 0xFFFFFFFF


def collect_strings(root: Element) -> list[str]:
    """Attribute names (mapped, first) then element names and string values."""
    mapped: list[str] = []
    rest: list[str] = ["", ANDROID_NS, "android"]

    def add(text: str) -> str:
        if text not in rest:
            rest.append(text)
        return text

    def walk(element: Element) -> None:
        for attribute in element.attributes:
            if attribute.android and attribute.name not in mapped:
                if attribute.name not in ATTR_IDS:
                    raise SystemExit(f"no resource ID recorded for android:{attribute.name}")
                mapped.append(attribute.name)
        add(element.name)
        for attribute in element.attributes:
            if not attribute.android:
                add(attribute.name)
            if attribute.kind == "string":
                add(str(attribute.value))
        for child in element.children:
            walk(child)

    walk(root)
    return mapped + rest


def encode_axml(root: Element) -> bytes:
    """Encode a manifest tree as binary Android XML."""
    strings = collect_strings(root)
    index_of = {text: position for position, text in enumerate(strings)}
    chunks = [encode_string_pool(strings)]
    mapped = [name for name in strings if name in ATTR_IDS]
    if mapped != strings[:len(mapped)]:
        raise SystemExit("resource map requires mapped attribute names first in the pool")
    chunks.append(struct.pack("<HHI", RES_XML_RESOURCE_MAP, 8, 8 + 4 * len(mapped))
                  + b"".join(struct.pack("<I", ATTR_IDS[name]) for name in mapped))

    def attribute_bytes(attribute: Attribute) -> bytes:
        raw, kind, data = typed_value(attribute, index_of.__getitem__)
        namespace = index_of[ANDROID_NS] if attribute.android else NO_STRING
        # ns, name, rawValue, then ResValue{size u16, res0 u8, dataType u8, data u32}.
        return struct.pack("<IIIHBBI", namespace, index_of[attribute.name], raw,
                           8, 0, kind, data)

    def emit(element: Element, depth: int) -> None:
        count = len(element.attributes)
        body = b"".join(attribute_bytes(attribute) for attribute in element.attributes)
        # Element names carry no namespace: only attributes use the android: URI.
        chunks.append(struct.pack("<HHIII", RES_XML_START_ELEMENT, 16, 36 + len(body),
                                  depth + 1, NO_STRING)
                      + struct.pack("<IIHHHHHH", NO_STRING, index_of[element.name],
                                    20, 20, count, 0, 0, 0) + body)
        for child in element.children:
            emit(child, depth + 1)
        chunks.append(struct.pack("<HHIIIII", RES_XML_END_ELEMENT, 16, 24, depth + 1,
                                  NO_STRING, NO_STRING, index_of[element.name]))

    chunks.insert(2, struct.pack("<HHIIIII", RES_XML_START_NAMESPACE, 16, 24, 1,
                                 NO_STRING, index_of["android"], index_of[ANDROID_NS]))
    emit(root, 0)
    chunks.append(struct.pack("<HHIIIII", RES_XML_END_NAMESPACE, 16, 24, 1, NO_STRING,
                              index_of["android"], index_of[ANDROID_NS]))
    body = b"".join(chunks)
    return struct.pack("<HHI", RES_XML, 8, 8 + len(body)) + body


def manifest() -> Element:
    """The manifest this APK ships. Values match docs and the real library contents."""
    return Element("manifest", [
        Attribute("package", "org.hematite.renderer", android=False),
        Attribute("versionCode", 3, "int"),
        Attribute("versionName", "0.3.0"),
    ], [
        Element("uses-sdk", [
            Attribute("minSdkVersion", 21, "int"),
            # 28 keeps v1-only signatures installable; see docs/KNOWN_LIMITATIONS.md.
            Attribute("targetSdkVersion", 28, "int"),
        ]),
        Element("application", [
            Attribute("label", "Hematite Renderer (bring-up)"),
            Attribute("hasCode", False, "bool"),
            Attribute("debuggable", True, "bool"),
            Attribute("allowBackup", False, "bool"),
            Attribute("extractNativeLibs", True, "bool"),
        ], [
            # Discovery only: PluginLoader queries ACTION_MAIN and reads this package's
            # meta-data. The class does not exist (hasCode=false, no dex), so starting
            # the activity would fail; the launcher never starts it.
            Element("activity", [
                Attribute("name", PLUGIN_ACTIVITY),
                Attribute("exported", True, "bool"),
                Attribute("label", "Hematite Renderer plugin"),
            ], [
                Element("intent-filter", [], [
                    Element("action", [
                        Attribute("name", "android.intent.action.MAIN"),
                    ]),
                ]),
            ]),
            Element("meta-data", [
                Attribute("name", "fclPlugin"),
                # A boolean typed value, not the string "true": Bundle.getBoolean only
                # reads TYPE_INT_BOOLEAN.
                Attribute("value", True, "bool"),
            ]),
            Element("meta-data", [
                Attribute("name", "renderer"),
                Attribute("value", f"{PLUGIN_ID}:{NATIVE_LIBRARY}:{NATIVE_LIBRARY}"),
            ]),
            Element("meta-data", [
                Attribute("name", "des"),
                Attribute("value", "Hematite (bring-up: fixed-function draws on Vulkan, no presentation)"),
            ]),
            Element("meta-data", [
                Attribute("name", "pojavEnv"),
                Attribute("value", PLUGIN_ENV),
            ]),
        ]),
    ])


# ---------------------------------------------------------------- ZIP + signing
@dataclass
class Entry:
    """One APK entry: uncompressed native libraries are page aligned like zipalign."""
    name: str
    data: bytes
    compress: bool = False
    align: int = 1


def write_zip(entries: list[Entry], sink: io.BufferedWriter) -> None:
    """Write entries with deterministic timestamps and 16 KB-aligned stored data."""
    with zipfile.ZipFile(sink, "w", allowZip64=False) as archive:
        for entry in entries:
            info = zipfile.ZipInfo(entry.name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED if entry.compress else zipfile.ZIP_STORED
            info.external_attr = 0o644 << 16
            info.create_system = 3
            if entry.align > 1:
                # Local header is 30 bytes plus the name and extra field.
                position = archive.fp.tell()
                header = 30 + len(info.filename.encode("utf-8"))
                pad = (-position - header) % entry.align
                info.extra = b"\0" * pad
            archive.writestr(info, entry.data)


def jar_signature(entries: list[Entry], certificate, private_key) -> list[Entry]:
    """Build the APK Signature Scheme v1 (JAR) META-INF entries."""
    signed = [entry for entry in entries if not entry.name.startswith("META-INF/")]
    manifest_lines = ["Manifest-Version: 1.0", "Built-By: Hematite build_apk.py",
                      "Created-By: Hematite (no JDK available in this environment)", ""]
    for entry in signed:
        digest = base64.b64encode(hashlib.sha256(entry.data).digest()).decode("ascii")
        manifest_lines += [f"Name: {entry.name}", f"SHA-256-Digest: {digest}", ""]
    manifest = "\r\n".join(manifest_lines).encode("utf-8")

    signature_lines = ["Signature-Version: 1.0",
                       "Created-By: Hematite (no JDK available in this environment)",
                       "SHA-256-Digest-Manifest: "
                       + base64.b64encode(hashlib.sha256(manifest).digest()).decode("ascii"), ""]
    for entry in signed:
        block = "\r\n".join([f"Name: {entry.name}",
                             "SHA-256-Digest: "
                             + base64.b64encode(hashlib.sha256(entry.data).digest()).decode("ascii"),
                             ""]).encode("utf-8")
        signature_lines += [f"Name: {entry.name}",
                            "SHA-256-Digest: "
                            + base64.b64encode(hashlib.sha256(block).digest()).decode("ascii"), ""]
    signature_file = "\r\n".join(signature_lines).encode("utf-8")

    der = (pkcs7.PKCS7SignatureBuilder()
           .set_data(signature_file)
           .add_signer(certificate, private_key, hashes.SHA256())
           .sign(serialization.Encoding.DER, [pkcs7.PKCS7Options.Binary,
                                              pkcs7.PKCS7Options.NoCapabilities]))
    return [Entry("META-INF/MANIFEST.MF", manifest),
            Entry("META-INF/CERT.SF", signature_file),
            Entry("META-INF/CERT.RSA", der)]


def length_prefixed(payload: bytes) -> bytes:
    return struct.pack("<I", len(payload)) + payload


def sequence_field(elements: list[bytes]) -> bytes:
    """AOSP `encodeAsSequenceOfLengthPrefixedElements`: concatenated lp() elements."""
    return b"".join(length_prefixed(element) for element in elements)


RSASSA_PKCS1_SHA256 = 0x0103
V2_BLOCK_ID = 0x7109871A
V2_MAGIC = b"APK Sig Block 42"
CHUNK = 1 << 20


def chunked_digest(sections: list[bytes]) -> bytes:
    """APK Signature Scheme v2 content digest: 1 MB chunks across the three sections."""
    digests = []
    for section in sections:
        for start in range(0, max(len(section), 1), CHUNK):
            piece = section[start:start + CHUNK]
            digests.append(hashlib.sha256(struct.pack("<B", 0xA5)
                                          + struct.pack("<I", len(piece)) + piece).digest())
    return hashlib.sha256(struct.pack("<B", 0x5A) + struct.pack("<I", len(digests))
                          + b"".join(digests)).digest()


def v2_signed_data(digest: bytes, certificate_der: bytes) -> bytes:
    """The `signed data` bytes that the RSA signature covers.

    Layout follows AOSP `V2SchemeSigner.generateApkSignatureSchemeV2Block` (fetched from
    `com/android/apksig/internal/apk/v2/V2SchemeSigner.java`): a sequence of four
    length-prefixed fields - digests, certificates, additional attributes (empty for v2)
    and a final empty element - with no outer length prefix. The signature is taken over
    exactly these bytes, which both AOSP's `V2SchemeVerifier` and apksigtool verify.
    """
    digests = sequence_field([struct.pack("<I", RSASSA_PKCS1_SHA256) + length_prefixed(digest)])
    certificates = sequence_field([certificate_der])
    return (length_prefixed(digests) + length_prefixed(certificates)
            + length_prefixed(b"") + length_prefixed(b""))


def build_signing_block(digest: bytes, certificate, private_key) -> bytes:
    """Construct the APK Signing Block holding one v2 signer."""
    certificate_der = certificate.public_bytes(serialization.Encoding.DER)
    signed_data = v2_signed_data(digest, certificate_der)
    signature = private_key.sign(signed_data, padding.PKCS1v15(), hashes.SHA256())
    public_key = certificate.public_key().public_bytes(
        serialization.Encoding.DER, serialization.PublicFormat.SubjectPublicKeyInfo)
    signer = (length_prefixed(signed_data)
              + length_prefixed(sequence_field(
                  [struct.pack("<I", RSASSA_PKCS1_SHA256) + length_prefixed(signature)]))
              + length_prefixed(public_key))
    # Block value: a length-prefixed sequence of length-prefixed signers.
    pair = struct.pack("<I", V2_BLOCK_ID) + length_prefixed(length_prefixed(signer))
    payload = struct.pack("<Q", len(pair)) + pair
    block_size = len(payload) + 8 + len(V2_MAGIC)
    return struct.pack("<Q", block_size) + payload + struct.pack("<Q", block_size) + V2_MAGIC


def find_eocd(blob: bytes) -> int:
    """Locate the End Of Central Directory record, rejecting commented archives."""
    for offset in range(len(blob) - 22, max(len(blob) - 22 - 0xFFFF, -1), -1):
        if blob[offset:offset + 4] == b"PK\x05\x06":
            return offset
    raise SystemExit("no ZIP End Of Central Directory record found")


def v2_digest_sections(blob: bytes) -> list[bytes]:
    """Byte ranges the v2 content digest covers.

    `blob` is the unsigned archive (entries, central directory, EOCD). AOSP digests
    exactly three ranges: the ZIP entries, the central directory, and the EOCD with its
    central-directory offset field treated as holding the offset of the signing block
    (which is where the block is inserted, so that value is the current CD offset).
    """
    eocd = find_eocd(blob)
    cd_offset, = struct.unpack_from("<I", blob, eocd + 16)
    if blob[cd_offset:cd_offset + 4] != b"PK\x01\x02":
        raise SystemExit("central directory offset does not point at a directory record")
    eocd_bytes = bytearray(blob[eocd:])
    struct.pack_into("<I", eocd_bytes, 16, cd_offset)
    return [blob[:cd_offset], blob[cd_offset:eocd], bytes(eocd_bytes)]


def build_apk(library: Path, status_text: str, output: Path) -> None:
    """Assemble, v1-sign and v2-sign the APK described by this module's docstring."""
    private_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "Hematite debug build"),
                      x509.NameAttribute(NameOID.ORGANIZATION_NAME, "Hematite")])
    now = datetime.datetime.now(datetime.timezone.utc)
    certificate = (x509.CertificateBuilder()
                   .subject_name(name).issuer_name(name)
                   .public_key(private_key.public_key())
                   .serial_number(x509.random_serial_number())
                   .not_valid_before(now - datetime.timedelta(days=1))
                   .not_valid_after(now + datetime.timedelta(days=3650))
                   .sign(private_key, hashes.SHA256()))

    entries = [
        Entry("AndroidManifest.xml", encode_axml(manifest())),
        Entry("assets/HEMATITE_STATUS.txt", status_text.encode("utf-8"), compress=True),
        Entry("lib/arm64-v8a/libhematite.so", library.read_bytes(), align=16384),
    ]
    entries += jar_signature(entries, certificate, private_key)

    buffer = io.BytesIO()
    write_zip(entries, buffer)
    blob = buffer.getvalue()
    cd_offset = struct.unpack_from("<I", blob, find_eocd(blob) + 16)[0]

    # The block's own length depends only on the digest and certificate lengths, which
    # are fixed, so a placeholder block gives the exact layout for the digest input.
    placeholder = build_signing_block(b"\0" * 32, certificate, private_key)
    digest = chunked_digest(v2_digest_sections(blob))
    block = build_signing_block(digest, certificate, private_key)
    if len(block) != len(placeholder):
        raise SystemExit("signing block layout changed with the real digest")

    final = bytearray(blob[:cd_offset] + block + blob[cd_offset:])
    eocd_offset = find_eocd(bytes(final))
    struct.pack_into("<I", final, eocd_offset + 16, cd_offset + len(block))
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(bytes(final))


def main() -> int:
    root = Path(__file__).resolve().parents[1]
    library = root / "target/aarch64-linux-android/release/libhematite_plugin.so"
    output = root / "dist/hematite-debug.apk"
    if not library.is_file():
        raise SystemExit(f"{library} is missing; run ./build.sh android first")
    status = (root / "docs/STATUS_REPORT.md").read_text(encoding="utf-8")
    build_apk(library, status, output)
    print(f"wrote {output} ({output.stat().st_size} bytes, "
          f"library {library.stat().st_size} bytes)")
    print("Next: python3 tools/verify_apk.py  (androguard + apksigtool + ELF checks)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
