//! Writing and reading a whole segment.
//!
//! `docs/SEGMENT_FORMAT.md` section 3 gives the layout and sections 13 and 14
//! give the two sequences this module implements.
//!
//! ```text
//! prologue            fixed 64 bytes
//! header              canonical CBOR
//! row group 0, row group 1, ...
//! index region
//! footer              canonical CBOR
//! footer trailer      fixed 32 bytes
//! ```
//!
//! **A reader starts at the end.** The trailer gives the footer offset, so a
//! reader opens a segment with one seek and one read, prunes by project, kind,
//! time, and statistics, and only then touches a page.
//!
//! **A segment never changes after the writer publishes it.** Everything here
//! either writes a whole file or fails; nothing edits one.

use std::collections::BTreeMap;

use super::format::*;
use super::index::{
    read_index, read_index_encrypted, write_index, write_index_encrypted, BlockFilter, ColumnIndex,
    TermPostings,
};
use super::page::{
    read_page, read_page_encrypted, split_into_pages, write_page, write_page_encrypted, Column,
};
use super::schema;
use crate::cbor::{self, MapBuilder, Value};
use crate::keys::Cipher;
use crate::row::EventRow;

/// What a segment says about itself before a reader touches a page.
///
/// Section 5: the header holds the facts that let a reader prune a segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub segment_id: [u8; 16],
    pub tablet_id: u64,
    pub virtual_shard: u64,
    pub workspace_id: [u8; 16],
    pub project_id: [u8; 16],
    /// The telemetry kinds this segment holds, so a query for one kind can
    /// reject a segment without reading a page.
    pub kinds: Vec<String>,
    /// D43. The class that bounds this segment's retention.
    pub retention_class: String,
    /// The minimum and maximum of each time basis. TallyOwl keeps three time
    /// facts and never collapses them, so a segment prunes on the one the query
    /// asked for.
    pub occurred_range: (i64, i64),
    pub received_range: (i64, i64),
    pub committed_range: (i64, i64),
    /// The first and last append-log position this segment covers.
    pub log_range: (u64, u64),
    pub row_count: u64,
    pub row_group_count: u64,
    pub uncompressed_bytes: u64,
    pub compressed_bytes: u64,
    pub projector_version: u64,
}

/// One row group's place in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowGroup {
    pub first_row: u64,
    pub rows: u64,
    /// Column name to the pages that hold it, each an offset and a row count.
    pub pages: BTreeMap<String, Vec<PageRef>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRef {
    pub offset: u64,
    pub rows: u64,
}

/// Section 8. What a reader needs before it selects a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footer {
    pub row_groups: Vec<RowGroup>,
    /// Column name to the offset of its index block.
    pub indexes: BTreeMap<String, (u64, IndexLayout)>,
    /// BLAKE3-256 over the data region, so a backup can be verified.
    pub payload_hash: [u8; 32],
    /// The key this segment's data and index regions are protected with, when
    /// they are. The footer holds a reference and never key material; the
    /// catalog holds the keys. See SEGMENT_FORMAT.md section 11 and D61.
    pub key_reference: Option<KeyReference>,
}

/// Which key opens a segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyReference {
    /// The algorithm number, so a later segment can use another one without a
    /// major version change.
    pub algorithm: u16,
    /// Which generation of the project key. Rotation writes a new generation
    /// and leaves already-written objects readable.
    pub generation: u32,
}

/// A segment, in memory.
///
/// The home profile's segments are tens of megabytes, so a whole one fits. A
/// cold reader that must fetch ranges reads the trailer and the footer and then
/// only the pages it needs; that path arrives with the cold tier.
#[derive(Debug, Clone)]
pub struct Segment {
    pub header: Header,
    pub footer: Footer,
    pub bytes: Vec<u8>,
    /// The content address from the prologue: BLAKE3-256 over every byte after
    /// it. This identifies the segment in a backup, a restore, and an object
    /// store.
    pub content_address: [u8; 32],
    /// The key this reader opens pages with, when the segment is protected.
    ///
    /// A segment opens without one: the prologue, the header, and the footer
    /// stay readable, so a query still prunes by project, kind, and time. Only
    /// reading a page or an index needs the key.
    pub cipher: Option<Cipher>,
}

/// Which columns get an exact index, and which layout each uses.
///
/// The writer selects a layout from measured statistics, and a wrong choice is
/// catastrophic in both directions: a unique-lookup layout on a repeated value
/// reaches a p99 of 4.7 milliseconds because every row shares one fingerprint,
/// and a term-postings layout on a unique column costs 27 bytes for each row,
/// which exceeds the 16 bytes of data it indexes. See D20.
fn choose_layout(name: &str, column: &Column) -> Option<IndexLayout> {
    let rows = column.len();
    if rows == 0 {
        return None;
    }

    // Only a correlation column is indexed. D20 makes built-in correlation IDs
    // always exact-indexed, and an ordinary descriptive column is a scan.
    let indexed = matches!(
        name,
        schema::EVENT_ID | schema::TRACE_ID | schema::SESSION_ID | schema::REQUEST_ID
    ) || name.starts_with(schema::PROPERTY_PREFIX);
    if !indexed {
        return None;
    }

    let distinct = distinct_estimate(column);
    // A value that repeats gets postings, because a filter cannot give a row
    // list and a query for a whole trace wants one.
    if distinct * 2 <= rows {
        return Some(IndexLayout::TermPostings);
    }
    // A unique value gets a filter. This is the default, and `unique-lookup`
    // now needs a measurement to justify itself.
    Some(IndexLayout::BlockFilter)
}

/// How many distinct values a column holds. Exact rather than estimated: a
/// segment is bounded, and an exact count removes a reason for a wrong layout.
fn distinct_estimate(column: &Column) -> usize {
    match column {
        Column::Identifiers(values) => values
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        Column::Text(values) => values
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        _ => column.len(),
    }
}

/// The bytes a value contributes to an index, or nothing when the row has none.
fn index_key(column: &Column, row: usize) -> Option<Vec<u8>> {
    match column {
        Column::Identifiers(values) => {
            let value = values.get(row)?;
            // An all-zero identifier is the absent form, and indexing it would
            // make every row without a trace share one term.
            (*value != [0u8; 16]).then(|| value.to_vec())
        }
        Column::Text(values) => values.get(row)?.as_ref().map(|v| v.as_bytes().to_vec()),
        Column::Unsigned(values) => Some(values.get(row)?.to_le_bytes().to_vec()),
        Column::Integers(values) | Column::Timestamps(values) => {
            Some(values.get(row)?.to_le_bytes().to_vec())
        }
        Column::Booleans(values) => Some(vec![u8::from(*values.get(row)?)]),
        Column::Floats(values) => Some(values.get(row)?.to_le_bytes().to_vec()),
        Column::Bytes(values) => values.get(row)?.clone(),
    }
}

/// Build one segment from rows.
///
/// The rows arrive in commit order and stay in it. Sorting rows inside a
/// segment does not change which segment holds them, and only routing and
/// compaction change that. See HIGH_CARDINALITY.md section 4.
pub struct SegmentWriter {
    pub segment_id: [u8; 16],
    pub tablet_id: u64,
    pub virtual_shard: u64,
    pub workspace_id: [u8; 16],
    pub project_id: [u8; 16],
    pub retention_class: String,
    pub log_range: (u64, u64),
    pub projector_version: u64,
    pub row_group_target_bytes: usize,
    pub page_target_bytes: usize,
    /// The project key, when this segment is protected. A home installation
    /// with no cold tier writes segments in the clear; a project whose data
    /// reaches an object store does not.
    pub cipher: Option<Cipher>,
}

impl SegmentWriter {
    pub fn new(
        segment_id: [u8; 16],
        workspace_id: [u8; 16],
        project_id: [u8; 16],
    ) -> SegmentWriter {
        SegmentWriter {
            segment_id,
            tablet_id: 0,
            virtual_shard: 0,
            workspace_id,
            project_id,
            retention_class: "detailed".to_string(),
            log_range: (0, 0),
            projector_version: 1,
            row_group_target_bytes: ROW_GROUP_TARGET_BYTES,
            page_target_bytes: PAGE_TARGET_BYTES,
            cipher: None,
        }
    }

    /// Write the whole segment. A segment with no row is refused, because a
    /// catalog entry for nothing is a file nobody can use.
    pub fn write(&self, rows: &[EventRow]) -> Result<Segment, FormatError> {
        if rows.is_empty() {
            return Err(FormatError::Unsupported(
                "A stored file needs at least one item.".to_string(),
            ));
        }

        let groups = self.split_rows(rows);
        let mut out = Vec::with_capacity(rows.len() * 64);

        // 1 and 2. The prologue, with a zero content address for now.
        out.extend_from_slice(MAGIC);
        put_u16(&mut out, FORMAT_MAJOR);
        put_u16(&mut out, FORMAT_MINOR);
        put_u32(&mut out, 0); // header length, filled in below
        put_u64(&mut out, 0); // header offset
        put_u64(&mut out, 0); // footer offset
        out.extend_from_slice(&[0u8; 32]);
        debug_assert_eq!(out.len(), PROLOGUE_BYTES);

        // 3. The header, then the row groups, then the index region.
        let header = self.build_header(rows, groups.len());
        let header_bytes = cbor::encode(&header_to_cbor(&header));
        let header_offset = out.len();
        out.extend_from_slice(&header_bytes);

        let mut row_groups = Vec::with_capacity(groups.len());
        let mut first_row = 0u64;
        let mut uncompressed_total = 0u64;

        // The index is built over the whole segment, so a probe answers once
        // rather than once for each row group. A block filter still holds one
        // filter for each row group, in row-group order, as section 7 requires.
        let all_columns = schema::to_columns(rows);
        let mut indexes: BTreeMap<String, ColumnIndex> = BTreeMap::new();
        for (name, column) in &all_columns.columns {
            let Some(layout) = choose_layout(name, column) else {
                continue;
            };
            indexes.insert(name.clone(), self.build_index(layout, column, &groups));
        }

        for group_rows in &groups {
            let columns = schema::to_columns(group_rows);
            let mut pages: BTreeMap<String, Vec<PageRef>> = BTreeMap::new();
            for (name, column) in &columns.columns {
                let mut refs = Vec::new();
                for piece in split_into_pages(column, self.page_target_bytes) {
                    let offset = out.len() as u64;
                    let rows_in_page = piece.len() as u64;
                    match &self.cipher {
                        None => {
                            write_page(&mut out, &piece);
                        }
                        Some(cipher) => {
                            write_page_encrypted(&mut out, &piece, cipher, self.segment_id)
                                .map_err(|e| FormatError::Unsupported(e.to_string()))?;
                        }
                    }
                    refs.push(PageRef {
                        offset,
                        rows: rows_in_page,
                    });
                }
                uncompressed_total += column.len() as u64;
                pages.insert(name.clone(), refs);
            }
            row_groups.push(RowGroup {
                first_row,
                rows: group_rows.len() as u64,
                pages,
            });
            first_row += group_rows.len() as u64;
        }

        let mut index_offsets: BTreeMap<String, (u64, IndexLayout)> = BTreeMap::new();
        for (name, index) in &indexes {
            let offset = out.len() as u64;
            match &self.cipher {
                None => {
                    write_index(&mut out, index);
                }
                Some(cipher) => {
                    write_index_encrypted(&mut out, index, cipher, self.segment_id)
                        .map_err(|e| FormatError::Unsupported(e.to_string()))?;
                }
            }
            index_offsets.insert(name.clone(), (offset, index.layout()));
        }

        // 4. The footer and the trailer.
        let payload_hash = content_address(&out[PROLOGUE_BYTES..]);
        let footer = Footer {
            row_groups,
            indexes: index_offsets,
            payload_hash,
            key_reference: self.cipher.as_ref().map(|cipher| KeyReference {
                algorithm: crate::keys::ALGORITHM_AES_256_GCM,
                generation: cipher.generation,
            }),
        };
        let footer_offset = out.len();
        let footer_bytes = cbor::encode(&footer_to_cbor(&footer));
        out.extend_from_slice(&footer_bytes);

        put_u64(&mut out, footer_offset as u64);
        put_u32(&mut out, footer_bytes.len() as u32);
        put_u64(&mut out, page_checksum(&footer_bytes));
        put_u32(&mut out, FORMAT_MINOR as u32);
        out.extend_from_slice(TRAILER_MAGIC);

        // 5. The content address, over every byte after the prologue.
        out[12..16].copy_from_slice(&(header_bytes.len() as u32).to_le_bytes());
        out[16..24].copy_from_slice(&(header_offset as u64).to_le_bytes());
        out[24..32].copy_from_slice(&(footer_offset as u64).to_le_bytes());
        let address = content_address(&out[PROLOGUE_BYTES..]);
        out[32..64].copy_from_slice(&address);

        let mut header = header;
        header.uncompressed_bytes = uncompressed_total;
        header.compressed_bytes = out.len() as u64;

        Ok(Segment {
            header,
            footer,
            bytes: out,
            content_address: address,
            cipher: self.cipher.clone(),
        })
    }

    /// Split rows into row groups on measured bytes.
    ///
    /// D49: the row-group count sets the cold request count for a column read
    /// directly, so it is a decision rather than a detail.
    fn split_rows<'a>(&self, rows: &'a [EventRow]) -> Vec<&'a [EventRow]> {
        // A rough size for each row, measured on a sample rather than assumed.
        // The exact figure does not matter; the order of magnitude does.
        let sample = rows.len().min(256);
        let measured = schema::to_columns(&rows[..sample]);
        let mut sample_bytes = 0usize;
        for column in measured.columns.values() {
            let mut probe = Vec::new();
            write_page(&mut probe, column);
            sample_bytes += probe.len();
        }
        let each_row = (sample_bytes as f64 / sample as f64).max(1.0);
        let rows_each_group = ((self.row_group_target_bytes as f64 / each_row) as usize).max(1);

        rows.chunks(rows_each_group).collect()
    }

    fn build_index(
        &self,
        layout: IndexLayout,
        column: &Column,
        groups: &[&[EventRow]],
    ) -> ColumnIndex {
        match layout {
            IndexLayout::BlockFilter => {
                // Section 7: one filter for each row group, in row-group order.
                let mut filters = Vec::with_capacity(groups.len());
                let mut row = 0usize;
                for group in groups {
                    let mut filter = BlockFilter::new(group.len());
                    for _ in 0..group.len() {
                        if let Some(key) = index_key(column, row) {
                            filter.insert(&key);
                        }
                        row += 1;
                    }
                    filters.push(filter);
                }
                ColumnIndex::BlockFilter(filters)
            }
            IndexLayout::TermPostings => {
                let mut postings = TermPostings::new();
                for row in 0..column.len() {
                    if let Some(key) = index_key(column, row) {
                        postings.add(&key, row as u32);
                    }
                }
                ColumnIndex::TermPostings(postings)
            }
            IndexLayout::UniqueLookup => {
                let mut lookup = super::index::UniqueLookup::new();
                for row in 0..column.len() {
                    if let Some(key) = index_key(column, row) {
                        lookup.add(&key, row as u32);
                    }
                }
                lookup.seal();
                ColumnIndex::UniqueLookup(lookup)
            }
        }
    }

    fn build_header(&self, rows: &[EventRow], row_groups: usize) -> Header {
        let mut kinds: Vec<String> = rows.iter().map(|r| r.kind.clone()).collect();
        kinds.sort();
        kinds.dedup();

        let range = |pick: fn(&EventRow) -> i64| {
            rows.iter().fold((i64::MAX, i64::MIN), |(low, high), row| {
                let at = pick(row);
                (low.min(at), high.max(at))
            })
        };

        Header {
            segment_id: self.segment_id,
            tablet_id: self.tablet_id,
            virtual_shard: self.virtual_shard,
            workspace_id: self.workspace_id,
            project_id: self.project_id,
            kinds,
            retention_class: self.retention_class.clone(),
            occurred_range: range(|r| r.occurred_at),
            received_range: range(|r| r.received_at),
            committed_range: range(|r| r.committed_at),
            log_range: self.log_range,
            row_count: rows.len() as u64,
            row_group_count: row_groups as u64,
            uncompressed_bytes: 0,
            compressed_bytes: 0,
            projector_version: self.projector_version,
        }
    }
}

// ---------------------------------------------------------------------------
// The header and the footer, as canonical CBOR
// ---------------------------------------------------------------------------

fn header_to_cbor(header: &Header) -> Value {
    let pair =
        |(low, high): (i64, i64)| Value::Array(vec![Value::integer(low), Value::integer(high)]);
    MapBuilder::new()
        .put("v", Value::Unsigned(FORMAT_MAJOR as u64))
        .put("id", Value::Bytes(header.segment_id.to_vec()))
        .put("tab", Value::Unsigned(header.tablet_id))
        .put("vs", Value::Unsigned(header.virtual_shard))
        .put("ws", Value::Bytes(header.workspace_id.to_vec()))
        .put("pr", Value::Bytes(header.project_id.to_vec()))
        .put(
            "kinds",
            Value::Array(header.kinds.iter().map(Value::text).collect()),
        )
        .put("ret", Value::text(&header.retention_class))
        .put("occ", pair(header.occurred_range))
        .put("rec", pair(header.received_range))
        .put("com", pair(header.committed_range))
        .put(
            "log",
            Value::Array(vec![
                Value::Unsigned(header.log_range.0),
                Value::Unsigned(header.log_range.1),
            ]),
        )
        .put("rows", Value::Unsigned(header.row_count))
        .put("rg", Value::Unsigned(header.row_group_count))
        .put("proj", Value::Unsigned(header.projector_version))
        .build()
}

fn header_from_cbor(value: &Value) -> Result<Header, FormatError> {
    let missing = |what: &str| {
        FormatError::Damaged(format!(
            "This stored file is missing the {what} it needs to be read."
        ))
    };
    let id = |name: &str| -> Result<[u8; 16], FormatError> {
        value
            .field(name)
            .and_then(|v| v.as_bytes())
            .and_then(|b| <[u8; 16]>::try_from(b).ok())
            .ok_or_else(|| missing("identifiers"))
    };
    let pair = |name: &str| -> Result<(i64, i64), FormatError> {
        let list = value
            .field(name)
            .and_then(|v| v.as_array())
            .ok_or_else(|| missing("time range"))?;
        Ok((
            list.first().and_then(|v| v.as_integer()).unwrap_or(0),
            list.get(1).and_then(|v| v.as_integer()).unwrap_or(0),
        ))
    };
    let count = |name: &str| -> u64 {
        value
            .field(name)
            .and_then(|v| v.as_unsigned())
            .unwrap_or_default()
    };

    // Rule 4: a reader refuses a version it does not know. It does not guess.
    let major = count("v");
    if major != FORMAT_MAJOR as u64 {
        return Err(FormatError::Unsupported(format!(
            "This stored data was written by a different version of TallyOwl \
             and this one cannot read it. It says version {major}, and this software reads version {FORMAT_MAJOR}."
        )));
    }

    Ok(Header {
        segment_id: id("id")?,
        tablet_id: count("tab"),
        virtual_shard: count("vs"),
        workspace_id: id("ws")?,
        project_id: id("pr")?,
        kinds: value
            .field("kinds")
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_text().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        retention_class: value
            .field("ret")
            .and_then(|v| v.as_text())
            .unwrap_or("detailed")
            .to_string(),
        occurred_range: pair("occ")?,
        received_range: pair("rec")?,
        committed_range: pair("com")?,
        log_range: {
            let list = value
                .field("log")
                .and_then(|v| v.as_array())
                .ok_or_else(|| missing("log range"))?;
            (
                list.first().and_then(|v| v.as_unsigned()).unwrap_or(0),
                list.get(1).and_then(|v| v.as_unsigned()).unwrap_or(0),
            )
        },
        row_count: count("rows"),
        row_group_count: count("rg"),
        uncompressed_bytes: 0,
        compressed_bytes: 0,
        projector_version: count("proj"),
    })
}

fn footer_to_cbor(footer: &Footer) -> Value {
    let groups: Vec<Value> = footer
        .row_groups
        .iter()
        .map(|group| {
            let pages: BTreeMap<String, Value> = group
                .pages
                .iter()
                .map(|(name, refs)| {
                    (
                        name.clone(),
                        Value::Array(
                            refs.iter()
                                .flat_map(|r| [Value::Unsigned(r.offset), Value::Unsigned(r.rows)])
                                .collect(),
                        ),
                    )
                })
                .collect();
            MapBuilder::new()
                .put("first", Value::Unsigned(group.first_row))
                .put("rows", Value::Unsigned(group.rows))
                .put("pages", Value::Map(pages))
                .build()
        })
        .collect();

    let indexes: BTreeMap<String, Value> = footer
        .indexes
        .iter()
        .map(|(name, (offset, layout))| {
            (
                name.clone(),
                Value::Array(vec![
                    Value::Unsigned(*offset),
                    Value::Unsigned(*layout as u64),
                ]),
            )
        })
        .collect();

    MapBuilder::new()
        .put("rg", Value::Array(groups))
        .put("idx", Value::Map(indexes))
        .put("hash", Value::Bytes(footer.payload_hash.to_vec()))
        // The key reference, never key material. An absent field means the
        // segment is not protected, which keeps an unencrypted segment exactly
        // the size it was.
        .put_some(
            "key",
            footer.key_reference.map(|key| {
                Value::Array(vec![
                    Value::Unsigned(key.algorithm as u64),
                    Value::Unsigned(key.generation as u64),
                ])
            }),
        )
        .build()
}

fn footer_from_cbor(value: &Value) -> Result<Footer, FormatError> {
    let damaged = |what: &str| {
        FormatError::Damaged(format!(
            "This stored file's {what} could not be read, so we cannot say what it holds."
        ))
    };

    let row_groups = value
        .field("rg")
        .and_then(|v| v.as_array())
        .ok_or_else(|| damaged("directory"))?
        .iter()
        .map(|group| {
            let pages = group
                .field("pages")
                .and_then(|v| v.as_map())
                .ok_or_else(|| damaged("directory"))?
                .iter()
                .map(|(name, list)| {
                    let numbers = list.as_array().ok_or_else(|| damaged("directory"))?;
                    let refs = numbers
                        .chunks(2)
                        .filter(|pair| pair.len() == 2)
                        .map(|pair| PageRef {
                            offset: pair[0].as_unsigned().unwrap_or(0),
                            rows: pair[1].as_unsigned().unwrap_or(0),
                        })
                        .collect();
                    Ok((name.clone(), refs))
                })
                .collect::<Result<BTreeMap<String, Vec<PageRef>>, FormatError>>()?;
            Ok(RowGroup {
                first_row: group
                    .field("first")
                    .and_then(|v| v.as_unsigned())
                    .unwrap_or(0),
                rows: group
                    .field("rows")
                    .and_then(|v| v.as_unsigned())
                    .unwrap_or(0),
                pages,
            })
        })
        .collect::<Result<Vec<RowGroup>, FormatError>>()?;

    let indexes = value
        .field("idx")
        .and_then(|v| v.as_map())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(name, pair)| {
                    let pair = pair.as_array()?;
                    let offset = pair.first()?.as_unsigned()?;
                    let layout = IndexLayout::from_number(pair.get(1)?.as_unsigned()? as u16)?;
                    Some((name.clone(), (offset, layout)))
                })
                .collect()
        })
        .unwrap_or_default();

    let payload_hash = value
        .field("hash")
        .and_then(|v| v.as_bytes())
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .ok_or_else(|| damaged("checksum"))?;

    Ok(Footer {
        row_groups,
        indexes,
        payload_hash,
        key_reference: value.field("key").and_then(|v| {
            let pair = v.as_array()?;
            Some(KeyReference {
                algorithm: pair.first()?.as_unsigned()? as u16,
                generation: pair.get(1)?.as_unsigned()? as u32,
            })
        }),
    })
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// Open a segment, following section 14.
///
/// 1. Read the trailer and verify the magic.
/// 2. Read and verify the footer.
/// 3. Read the header and check the format version.
///
/// A file that does not end with the trailer magic is incomplete, and recovery
/// treats it as a partial write rather than as damage.
pub fn open(bytes: Vec<u8>, verify: bool) -> Result<Segment, FormatError> {
    if bytes.len() < PROLOGUE_BYTES + TRAILER_BYTES {
        return Err(FormatError::Incomplete(
            "This stored file is too short to be one of ours. It was probably not finished being written."
                .to_string(),
        ));
    }
    if &bytes[0..8] != MAGIC {
        return Err(FormatError::Incomplete(
            "This file is not one of ours.".to_string(),
        ));
    }

    let trailer_at = bytes.len() - TRAILER_BYTES;
    if &bytes[bytes.len() - 8..] != TRAILER_MAGIC {
        return Err(FormatError::Incomplete(
            "This stored file was not finished being written, so we did not use it.".to_string(),
        ));
    }

    let footer_offset = get_u64(&bytes, trailer_at)? as usize;
    let footer_length = get_u32(&bytes, trailer_at + 8)? as usize;
    let footer_checksum = get_u64(&bytes, trailer_at + 12)?;

    let footer_bytes = slice(&bytes, footer_offset, footer_length)?;
    if verify && page_checksum(footer_bytes) != footer_checksum {
        return Err(FormatError::Damaged(
            "This stored file's directory did not read back as what it was written as. \
             We cannot say what it holds, so we did not answer from it."
                .to_string(),
        ));
    }
    let footer = footer_from_cbor(&cbor::decode(footer_bytes).map_err(|e| {
        FormatError::Damaged(format!(
            "This stored file's directory could not be read. {e}"
        ))
    })?)?;

    let major = get_u16(&bytes, 8)?;
    if major != FORMAT_MAJOR {
        return Err(FormatError::Unsupported(format!(
            "This stored data was written by a different version of TallyOwl and this one cannot read it. \
             It says version {major}, and this software reads version {FORMAT_MAJOR}."
        )));
    }

    let header_length = get_u32(&bytes, 12)? as usize;
    let header_offset = get_u64(&bytes, 16)? as usize;
    let header_bytes = slice(&bytes, header_offset, header_length)?;
    let mut header = header_from_cbor(&cbor::decode(header_bytes).map_err(|e| {
        FormatError::Damaged(format!(
            "This stored file's description could not be read. {e}"
        ))
    })?)?;
    header.compressed_bytes = bytes.len() as u64;

    let mut content_address = [0u8; 32];
    content_address.copy_from_slice(&bytes[32..64]);

    Ok(Segment {
        header,
        footer,
        bytes,
        content_address,
        cipher: None,
    })
}

/// Open a protected segment.
///
/// The key reference in the footer says which key, and the caller resolves it
/// from the catalog. A segment whose key is destroyed still opens and still
/// prunes; it simply cannot produce a row.
pub fn open_with_key(bytes: Vec<u8>, verify: bool, cipher: Cipher) -> Result<Segment, FormatError> {
    let mut segment = open(bytes, verify)?;
    segment.cipher = Some(cipher);
    Ok(segment)
}

impl Segment {
    /// Whether the bytes on disk are the bytes that were written.
    ///
    /// This is what `scrub` runs and what a restore checks before it publishes
    /// a snapshot. A restore never silently skips a file.
    pub fn verify(&self) -> Result<(), FormatError> {
        if content_address(&self.bytes[PROLOGUE_BYTES..]) != self.content_address {
            return Err(FormatError::Damaged(format!(
                "The stored file {} is not the file it says it is. \
                 Its contents changed after it was written.",
                crate::row::hex(&self.header.segment_id)
            )));
        }
        Ok(())
    }

    /// Every row in this segment, in commit order.
    ///
    /// A whole-segment read is the compaction and export path. A query reads
    /// only the pages it needs.
    pub fn rows(&self, verify: bool) -> Result<Vec<EventRow>, FormatError> {
        let mut out = Vec::with_capacity(self.header.row_count as usize);
        for group in &self.footer.row_groups {
            out.extend(self.row_group(group, verify)?);
        }
        Ok(out)
    }

    fn row_group(&self, group: &RowGroup, verify: bool) -> Result<Vec<EventRow>, FormatError> {
        let mut columns: BTreeMap<String, Column> = BTreeMap::new();
        for (name, refs) in &group.pages {
            let mut merged: Option<Column> = None;
            for page_ref in refs {
                let at = page_ref.offset as usize;
                let page = match &self.cipher {
                    None => read_page(&self.bytes, at, verify)?,
                    Some(cipher) => read_page_encrypted(
                        &self.bytes,
                        at,
                        verify,
                        cipher,
                        self.header.segment_id,
                    )?,
                };
                merged = Some(match merged {
                    None => page.column,
                    Some(existing) => append(existing, page.column)?,
                });
            }
            if let Some(column) = merged {
                columns.insert(name.clone(), column);
            }
        }
        Ok(schema::to_rows(
            &columns,
            group.rows as usize,
            self.header.workspace_id,
            self.header.project_id,
        ))
    }

    /// One column's index, when the segment has one for it.
    pub fn index(&self, column: &str, verify: bool) -> Result<Option<ColumnIndex>, FormatError> {
        let Some((offset, _)) = self.footer.indexes.get(column) else {
            return Ok(None);
        };
        match &self.cipher {
            None => read_index(&self.bytes, *offset as usize, verify).map(Some),
            Some(cipher) => read_index_encrypted(
                &self.bytes,
                *offset as usize,
                verify,
                cipher,
                self.header.segment_id,
            )
            .map(Some),
        }
    }

    /// Whether this segment's data and index regions are protected.
    pub fn is_encrypted(&self) -> bool {
        self.footer.key_reference.is_some()
    }

    /// Which key opens it, when it is protected.
    pub fn key_reference(&self) -> Option<KeyReference> {
        self.footer.key_reference
    }

    /// Whether this segment can hold a value, without reading a page.
    ///
    /// `false` is exact: the value is definitely absent. `true` means the
    /// caller should read the rows and verify, which is what makes a filter
    /// safe.
    pub fn may_hold(&self, column: &str, value: &[u8], verify: bool) -> bool {
        match self.index(column, verify) {
            Ok(Some(ColumnIndex::BlockFilter(filters))) => {
                filters.iter().any(|filter| filter.maybe(value))
            }
            Ok(Some(ColumnIndex::TermPostings(postings))) => postings.rows(value).is_some(),
            Ok(Some(ColumnIndex::UniqueLookup(lookup))) => !lookup.candidates(value).is_empty(),
            // A segment with no index for the column cannot rule the value out,
            // and a damaged index is not a reason to answer "no". Both say
            // maybe, and the row read then decides.
            _ => true,
        }
    }

    /// Whether a time range can intersect this segment, by one basis.
    pub fn overlaps(&self, basis: crate::store::TimeBasis, start: i64, end: i64) -> bool {
        let (low, high) = match basis {
            crate::store::TimeBasis::OccurredAt => self.header.occurred_range,
            crate::store::TimeBasis::ReceivedAt => self.header.received_range,
            crate::store::TimeBasis::CommittedAt => self.header.committed_range,
        };
        low < end && high >= start
    }
}

/// Join two pages of one column back into one.
fn append(left: Column, right: Column) -> Result<Column, FormatError> {
    Ok(match (left, right) {
        (Column::Timestamps(mut a), Column::Timestamps(b)) => {
            a.extend(b);
            Column::Timestamps(a)
        }
        (Column::Integers(mut a), Column::Integers(b)) => {
            a.extend(b);
            Column::Integers(a)
        }
        (Column::Unsigned(mut a), Column::Unsigned(b)) => {
            a.extend(b);
            Column::Unsigned(a)
        }
        (Column::Floats(mut a), Column::Floats(b)) => {
            a.extend(b);
            Column::Floats(a)
        }
        (Column::Booleans(mut a), Column::Booleans(b)) => {
            a.extend(b);
            Column::Booleans(a)
        }
        (Column::Identifiers(mut a), Column::Identifiers(b)) => {
            a.extend(b);
            Column::Identifiers(a)
        }
        (Column::Text(mut a), Column::Text(b)) => {
            a.extend(b);
            Column::Text(a)
        }
        (Column::Bytes(mut a), Column::Bytes(b)) => {
            a.extend(b);
            Column::Bytes(a)
        }
        _ => {
            return Err(FormatError::Damaged(
                "Two parts of one stored column do not agree on what they hold.".to_string(),
            ))
        }
    })
}
