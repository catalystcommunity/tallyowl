# Segment and manifest format

This document uses these abbreviations: Concise Binary Object Representation
(CBOR), CBOR Service Interface Language (CSIL), write-ahead log (WAL), and
least significant bit (LSB). [DOCUMENTATION.md](DOCUMENTATION.md) holds the
full list.

## 1. Purpose

A TallyOwl Segment is the physical unit of stored telemetry. It is immutable.
It is self-describing. It carries the data and the indexes that a query needs.

This document gives the byte layout. [STORAGE.md](STORAGE.md) gives the write
path, the recovery sequence, and the tier model.

The format is the contract that outlives every implementation. This document
gives a reader everything that it needs to open a segment.

## 2. Rules

1. Every multi-byte integer uses little-endian byte order.
2. Every offset is a byte offset from the start of the segment.
3. Every length is a byte count.
4. A reader refuses a version that it does not know. It does not guess.
5. A reader verifies a checksum before it trusts a byte.
6. A reader allocates only after it validates a declared length.
7. A segment never changes after the writer publishes it.

## 3. Layout

```text
+--------------------------------------------------+
| prologue            fixed 64 bytes               |
+--------------------------------------------------+
| header              canonical CBOR               |
+--------------------------------------------------+
| row group 0                                      |
|   column page 0, column page 1, ...              |
| row group 1                                      |
|   ...                                            |
+--------------------------------------------------+
| index region                                     |
|   term dictionary, postings, fingerprint runs    |
+--------------------------------------------------+
| footer              canonical CBOR               |
+--------------------------------------------------+
| footer trailer      fixed 32 bytes               |
+--------------------------------------------------+
```

A reader starts at the end. The footer trailer gives the footer offset, so a
reader opens a segment with one seek and one read.

## 4. Prologue

The prologue is 64 bytes and never changes size.

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 8 | Magic. The ASCII bytes `TOWLSEG1`. |
| 8 | 2 | Format major version. A reader refuses an unknown major. |
| 10 | 2 | Format minor version. A reader accepts an unknown minor. |
| 12 | 4 | Header length. |
| 16 | 8 | Header offset. |
| 24 | 8 | Footer offset. |
| 32 | 32 | Segment content address. BLAKE3-256 of every byte after the prologue. |

A major version change means an incompatible layout. A minor version change
adds an optional field that an older reader can skip.

## 5. Header

The header is canonical CBOR. It holds the facts that let a reader prune a
segment without reading a page.

- format and schema version;
- segment ID;
- tablet ID and virtual shard ID;
- workspace ID and project ID;
- telemetry kind;
- retention class (D43);
- time range, as the minimum and maximum of each time basis;
- first and last source log position;
- row count and row-group count;
- uncompressed and compressed byte counts;
- projector name, projector version, and source lineage when derived;
- the column schema.

A column schema entry holds a stable numeric column ID, a value type, a
physical access class from D20, and the encoding.

A column ID is stable. Assign it one time. Never use it for a different field.

## 6. Row groups and pages

A row group holds a bounded count of rows. Each column contributes one page for
each row group.

A page holds:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 4 | Page length, not including this header |
| 4 | 2 | Encoding |
| 6 | 2 | Compression codec |
| 8 | 4 | Row count |
| 12 | 4 | Uncompressed length |
| 16 | 8 | Page checksum. xxHash3-64 of the compressed bytes. |
| 24 | n | Null bitmap, one bit for each row, LSB first |
| 24+n | m | Encoded and compressed values |

A page is independently readable. A query decodes only the pages that it needs.

### Encodings

Version 1 uses simple encodings. A benchmark must justify anything more
elaborate. See D17.

| Encoding | Applies to |
| --- | --- |
| `plain-fixed` | Boolean, float, and fixed-width identifier |
| `varint` | Unsigned integer |
| `varint-delta` | Timestamp and a sorted integer |
| `offset-bytes` | Text and bytes, as an offset array plus one byte region |
| `dictionary` | A column where a dictionary measurably reduces the page |

The compression codec is `none` or Zstandard. The writer uses Zstandard level 1
for hot and warm data and may use level 3 during cold compaction.

The page target is 64 KiB compressed. A reader accepts every permitted size.

## 7. Index region

The index region serves exact lookup on high-cardinality values. Each
exact-indexed column chooses a layout from measured statistics. See
[HIGH_CARDINALITY.md](HIGH_CARDINALITY.md).

| Layout | Applies to |
| --- | --- |
| `term-postings` | A repeated value. A prefix-compressed term dictionary plus compressed row-ID postings. |
| `unique-lookup` | A mostly-unique value. Sorted fixed-width fingerprints plus row IDs. |
| `ordered-values` | A range query. Sorted value and row blocks with skip data. |
| `facet-column` | Grouping. Typed values or ordinals for local partial aggregation. |

A fingerprint prunes. It never decides. The reader verifies the full typed
value before it returns a row. A collision therefore cannot produce an
incorrect result.

Each index block carries its own length, encoding, and xxHash3-64 checksum.

## 8. Footer

The footer is canonical CBOR. It holds:

- column statistics: minimum, maximum, null count, and distinct estimate;
- dictionaries that pages share;
- optional Bloom filters for segment rejection;
- the row-group directory, with an offset and a row count for each;
- the index directory, with an offset and a layout for each column;
- the payload hash, BLAKE3-256 over the data region;
- the encryption key reference, when the segment holds encrypted data.

## 9. Footer trailer

The trailer is the last 32 bytes of the file. A reader reads it first.

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 8 | Footer offset |
| 8 | 4 | Footer length |
| 12 | 8 | Footer checksum. xxHash3-64 of the footer bytes. |
| 20 | 4 | Format minor version, repeated for a truncation check |
| 24 | 8 | Magic. The ASCII bytes `TOWLEND1`. |

A file that does not end with the trailer magic is incomplete. Recovery treats
it as a partial write.

## 10. Checksums and identity

Two functions do two different jobs. See D44.

| Use | Function | Reason |
| --- | --- | --- |
| Page and index block | xxHash3-64 | Corruption detection on the hot path |
| Footer | xxHash3-64 | Corruption detection |
| Segment content address | BLAKE3-256 | Identity across backup, restore, and cold storage |

A page checksum finds a damaged read. It does not defend against an attacker.

A content address identifies a segment in a backup, a restore, and an object
store. It needs collision resistance.

## 11. Encryption

A segment can encrypt its data region. The cold tier needs this, because
erasure destroys key material instead of rewriting an object. See D28.

The footer holds a key reference, never key material. The catalog holds the
keys.

An encrypted segment keeps its header readable, so a reader can still prune by
time, project, and kind without a key.

The key design is separate work and gates cold tiering for a project that
permits erasure.

## 12. Manifest

A manifest describes one segment and lives beside it. The catalog holds a copy,
and a repair command rebuilds the catalog by scanning manifests.

A manifest is canonical CBOR and holds:

- the segment ID and the content address;
- the tablet, virtual shard, workspace, project, and kind;
- the time range and the log position range;
- the row count and the byte counts;
- the generation that published it;
- the tier location, local or cold, and the object key when cold;
- the checksum of the manifest itself.

The manifest is authoritative. A directory path is a convenience and never
carries query meaning.

## 13. Writing and publishing

1. Write to a temporary name.
2. Write the prologue with a zero content address.
3. Write the header, the row groups, and the index region.
4. Write the footer and the trailer.
5. Compute the content address and write it into the prologue.
6. Call fsync on the file.
7. Rename the file atomically.
8. Publish the segment in one catalog transaction.

A crash can leave a complete file with no catalog reference. Recovery verifies
the file and then adopts it or removes it. The catalog must never refer to a
partial file.

## 14. Reading

1. Read the trailer and verify the magic.
2. Read and verify the footer.
3. Read the header and check the format version.
4. Prune by project, kind, time, and statistics.
5. Select the required columns and row groups.
6. Read only the required pages and verify each checksum.
7. Apply the visible tombstone generation.

A reader never decompresses before it validates a declared length. A
decompression ratio limit applies.

## 15. Compatibility

- A minor version adds an optional field. An older reader skips it.
- A major version changes the layout. An older reader refuses the file.
- A new encoding needs a new encoding number and a minor version increase.
- A reader that meets an unknown encoding fails for that column. It does not
  return a wrong value.
- Startup refuses an unknown incompatible version rather than guessing.

## 16. Required tests

1. A torn write at every structural boundary.
2. A truncated file with no trailer.
3. A corrupted page, index block, and footer, each detected.
4. A segment written by one version and read by an adjacent version.
5. An unknown encoding in one column, with other columns still readable.
6. A content address that does not match the bytes.
7. A decompression bomb rejected by the ratio limit.
8. A read of an encrypted segment after key destruction.
9. A catalog rebuilt by scanning manifests only.
