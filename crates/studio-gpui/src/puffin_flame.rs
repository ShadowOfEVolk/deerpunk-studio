//! Data layer of the in-studio puffin profiler: a 1:1 port of the pure,
//! UI-independent parts of the vendored `third_party/puffin_egui` (lib.rs
//! `SelectedFrames`/`Streams`, flamegraph.rs `Options`/`Sorting` + layout
//! walking, stats.rs aggregation, filter.rs `Filter`).
//!
//! Everything here is plain Rust over `puffin` types so it is unit-testable
//! without gpui: synthetic `UnpackedFrameData` frames are built from raw
//! `puffin::Stream` scope events. The gpui side (puffin.rs) only renders
//! `FlameLayout` geometry and drives the controls.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use puffin::{
    FrameIndex, MergeScope, NanoSecond, Reader, Scope, ScopeCollection, ScopeId, Stream,
    StreamInfo, ThreadInfo, UnpackedFrameData, merge_scopes_for_thread,
};

// ---------------------------------------------------------------------------
// filter.rs
// ---------------------------------------------------------------------------

/// Case-insensitive substring filter (vendor `Filter`).
#[derive(Clone, Debug, Default)]
pub struct Filter {
    filter: String,
}

impl Filter {
    pub fn set(&mut self, filter: impl Into<String>) {
        self.filter = filter.into().to_lowercase();
    }

    /// Normalized (lowercased) filter text.
    pub fn as_str(&self) -> &str {
        &self.filter
    }

    /// `true` = show everything.
    pub fn is_empty(&self) -> bool {
        self.filter.is_empty()
    }

    pub fn include(&self, id: &str) -> bool {
        self.filter.is_empty() || id.to_lowercase().contains(&self.filter)
    }
}

// ---------------------------------------------------------------------------
// lib.rs: SelectedFrames / Streams
// ---------------------------------------------------------------------------

/// Vendor `Streams`: all selected frames' streams for one thread, plus the
/// merged scopes for the merge view and the nesting depth.
#[derive(Clone)]
pub struct ThreadStreams {
    pub streams: Vec<Arc<StreamInfo>>,
    pub merged_scopes: Vec<MergeScope<'static>>,
    pub max_depth: usize,
}

/// Vendor `SelectedFrames`: a non-empty set of unpacked frames ready to be
/// viewed, with the raw/merged nanosecond ranges and per-thread data.
#[derive(Clone)]
pub struct SelectedFrames {
    /// Sorted by frame index, deduplicated. Never empty (vendor `Vec1`).
    pub frames: Vec<Arc<UnpackedFrameData>>,
    pub raw_range_ns: (NanoSecond, NanoSecond),
    pub merged_range_ns: (NanoSecond, NanoSecond),
    pub threads: BTreeMap<ThreadInfo, ThreadStreams>,
}

impl SelectedFrames {
    /// Vendor `SelectedFrames::try_from_iter`: `None` on empty input.
    pub fn try_from_iter(
        scope_collection: &ScopeCollection,
        frames: impl Iterator<Item = Arc<UnpackedFrameData>>,
    ) -> Option<Self> {
        let frames: Vec<_> = frames.collect();
        if frames.is_empty() {
            None
        } else {
            Some(Self::from_frames(scope_collection, frames))
        }
    }

    /// Vendor `SelectedFrames::from_vec1` (the non-empty path).
    pub fn from_frames(
        scope_collection: &ScopeCollection,
        mut frames: Vec<Arc<UnpackedFrameData>>,
    ) -> Self {
        debug_assert!(!frames.is_empty());
        frames.sort_by_key(|f| f.frame_index());
        frames.dedup_by_key(|f| f.frame_index());

        let mut threads: BTreeSet<ThreadInfo> = BTreeSet::new();
        for frame in &frames {
            for ti in frame.thread_streams.keys() {
                threads.insert(ti.clone());
            }
        }

        let threads: BTreeMap<ThreadInfo, ThreadStreams> = threads
            .into_iter()
            .map(|ti| (ti.clone(), Self::streams(scope_collection, &frames, &ti)))
            .collect();

        let mut merged_min_ns = NanoSecond::MAX;
        let mut merged_max_ns = NanoSecond::MIN;
        for stream in threads.values() {
            for scope in &stream.merged_scopes {
                let scope_start = scope.relative_start_ns;
                let scope_end = scope_start + scope.duration_per_frame_ns;
                merged_min_ns = merged_min_ns.min(scope_start);
                merged_max_ns = merged_max_ns.max(scope_end);
            }
        }

        let raw_range_ns = (
            frames.first().expect("non-empty").range_ns().0,
            frames.last().expect("non-empty").range_ns().1,
        );

        Self {
            frames,
            raw_range_ns,
            merged_range_ns: (merged_min_ns, merged_max_ns),
            threads,
        }
    }

    /// Vendor `Streams::new` for one thread.
    fn streams(
        scope_collection: &ScopeCollection,
        frames: &[Arc<UnpackedFrameData>],
        thread_info: &ThreadInfo,
    ) -> ThreadStreams {
        let streams: Vec<_> = frames
            .iter()
            .filter_map(|frame| frame.thread_streams.get(thread_info).cloned())
            .collect();

        let merged_scopes = merge_scopes_for_thread(scope_collection, frames, thread_info)
            .unwrap_or_default()
            .into_iter()
            .map(MergeScope::into_owned)
            .collect();

        let max_depth = streams.iter().map(|s| s.depth).max().unwrap_or(0);

        ThreadStreams {
            streams,
            merged_scopes,
            max_depth,
        }
    }

    /// Vendor `SelectedFrames::contains` (exercised by unit tests).
    #[allow(dead_code)]
    pub fn contains(&self, frame_index: FrameIndex) -> bool {
        self.frames.iter().any(|f| f.frame_index() == frame_index)
    }
}

// ---------------------------------------------------------------------------
// flamegraph.rs: Options / Sorting
// ---------------------------------------------------------------------------

/// Vendor `flamegraph::Options`, trimmed to what the gpui port renders.
/// Nanosecond zoom state uses f64 (vendor f32) for headroom on long frames.
#[derive(Clone)]
pub struct FlameOptions {
    /// Controls zoom: nanoseconds mapped onto the full canvas width.
    pub canvas_width_ns: f64,
    /// How much we have panned sideways, in pixels.
    pub sideways_pan_px: f32,
    /// Aggregate child scopes with the same id+data?
    pub merge_scopes: bool,
    /// Vendor `Sorting`: sort threads by name (false = by start time).
    pub sort_by_name: bool,
    pub sort_reversed: bool,
    /// Per-thread visibility (vendor `flamegraph_threads.show` flags).
    pub hidden_threads: BTreeSet<String>,
    /// Per-thread collapse (vendor `flamegraph_collapse` flags).
    pub collapsed_threads: BTreeSet<String>,
    /// Vendor `Options::scope_name_filter`.
    pub filter: Filter,
}

impl Default for FlameOptions {
    fn default() -> Self {
        Self {
            canvas_width_ns: 0.0,
            sideways_pan_px: 0.0,
            merge_scopes: false, // vendor default: only works well single-threaded
            sort_by_name: false,
            sort_reversed: false,
            hidden_threads: BTreeSet::new(),
            collapsed_threads: BTreeSet::new(),
            filter: Filter::default(),
        }
    }
}

impl FlameOptions {
    /// Vendor `Sorting::sort`: thread order for the canvas.
    pub fn sorted_threads(&self, mut threads: Vec<ThreadInfo>) -> Vec<ThreadInfo> {
        if self.sort_by_name {
            // vendor uses natord (natural order); plain case-insensitive byte
            // order is the closest dependency-free match.
            threads.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        } else {
            threads.sort_by_key(|info| info.start_time_ns);
        }
        if self.sort_reversed {
            threads.reverse();
        }
        threads
    }

    /// Vendor `paint_record` click: zoom the canvas onto a scope range
    /// (relative to the canvas start). `canvas_width_px` is the current
    /// canvas width in points.
    pub fn zoom_to(
        &mut self,
        relative_range: (NanoSecond, NanoSecond),
        canvas_width_px: f32,
        canvas_start_ns: NanoSecond,
    ) {
        let width_ns = (relative_range.1 - relative_range.0).max(1) as f64;
        self.canvas_width_ns = width_ns;
        self.sideways_pan_px = -(canvas_width_px as f64
            * (relative_range.0 - canvas_start_ns) as f64
            / width_ns) as f32;
    }

    /// Vendor double-click reset: the full range across the canvas.
    pub fn reset_view(&mut self, total_ns: (NanoSecond, NanoSecond)) {
        self.canvas_width_ns = (total_ns.1 - total_ns.0).max(1) as f64;
        self.sideways_pan_px = 0.0;
    }
}

// ---------------------------------------------------------------------------
// Layout: pure geometry for painting + hit-testing (gpui-free)
// ---------------------------------------------------------------------------

/// Vendor `Options` visual constants.
pub const TEXT_HEIGHT: f32 = 15.0;
pub const RECT_HEIGHT: f32 = 16.0;
pub const SPACING: f32 = 4.0;

/// One paintable scope (raw `Scope` or merged `MergeScope`) flattened into
/// canvas-space nanoseconds. Vendor paints these while traversing streams;
/// we pre-walk once per frame-version so paint + hit-test share the geometry.
pub struct ScopeEntry {
    pub start_ns: NanoSecond,
    pub end_ns: NanoSecond,
    pub id: ScopeId,
    /// Scope dynamic data (`''` if none).
    pub data: String,
    pub depth: usize,
    /// Index into [`FlameLayout::threads`].
    pub thread_ix: usize,
    /// `0` for a raw scope; merge piece count for merged scopes.
    pub merge_pieces: usize,
    /// Direct children count (vendor tooltip "children: N").
    pub child_count: usize,
}

impl ScopeEntry {
    pub fn duration_ns(&self) -> NanoSecond {
        self.end_ns - self.start_ns
    }
}

/// Where a thread block sits vertically in the canvas.
pub struct ThreadLayout {
    pub info: ThreadInfo,
    /// y of the first scope row (below the thread label row).
    pub scopes_top_y: f32,
    pub max_depth: usize,
    pub collapsed: bool,
}

impl ThreadLayout {
    /// Rect of the clickable "⏵ name" label row (vendor `paint_thread_info`).
    pub fn label_y(&self) -> f32 {
        self.scopes_top_y - TEXT_HEIGHT
    }

    /// Estimated label width: collapse glyph + name, ~7px/char at 11px font.
    pub fn label_width(&self) -> f32 {
        (self.info.name.len() as f32 + 2.0) * 7.0
    }
}

/// Canvas-space layout of the current selection: thread blocks + every
/// scope as ns ranges + depths. x-mapping is applied on demand from
/// [`FlameOptions`] zoom/pan so panning/zooming never rebuilds this.
pub struct FlameLayout {
    pub threads: Vec<ThreadLayout>,
    pub scopes: Vec<ScopeEntry>,
    /// Canvas-left nanosecond (raw or merged range start, per view mode).
    pub start_ns: NanoSecond,
    /// Total content height in px (timeline row + thread blocks).
    pub height: f32,
}

impl FlameLayout {
    /// Walk all selected frames' scopes (raw or merged, per `opts`) into
    /// canvas geometry. Mirrors vendor `ui_canvas`.
    pub fn build(sel: &SelectedFrames, opts: &FlameOptions) -> Self {
        let start_ns = if opts.merge_scopes {
            sel.merged_range_ns.0
        } else {
            sel.raw_range_ns.0
        };

        let mut threads = Vec::new();
        let mut scopes = Vec::new();
        let mut cursor_y = TEXT_HEIGHT; // timeline label row on top

        for info in opts.sorted_threads(sel.threads.keys().cloned().collect()) {
            if opts.hidden_threads.contains(&info.name) {
                continue;
            }
            let thread_data = &sel.threads[&info];
            let collapsed = opts.collapsed_threads.contains(&info.name);

            // Visual separator between threads (vendor paints a line here).
            cursor_y += 2.0;
            cursor_y += 2.0;
            let scopes_top_y = cursor_y + TEXT_HEIGHT;

            if !collapsed {
                let thread_ix = threads.len();
                if opts.merge_scopes {
                    for merge in &thread_data.merged_scopes {
                        walk_merge(merge, 0, thread_ix, &mut scopes);
                    }
                } else {
                    for stream_info in &thread_data.streams {
                        let top_scopes =
                            match Reader::from_start(&stream_info.stream).read_top_scopes() {
                                Ok(scopes) => scopes,
                                Err(_) => continue,
                            };
                        for scope in top_scopes {
                            walk_raw(&stream_info.stream, &scope, 0, thread_ix, &mut scopes);
                        }
                    }
                }
                cursor_y = scopes_top_y + thread_data.max_depth as f32 * (RECT_HEIGHT + SPACING);
            }

            threads.push(ThreadLayout {
                info,
                scopes_top_y,
                max_depth: thread_data.max_depth,
                collapsed,
            });
            cursor_y += TEXT_HEIGHT; // extra spacing between threads
        }

        let height = cursor_y.max(TEXT_HEIGHT * 2.0);
        Self {
            threads,
            scopes,
            start_ns,
            height,
        }
    }

    /// Pixel x range of a scope under the current zoom/pan.
    fn scope_x(&self, opts: &FlameOptions, canvas_width_px: f32, e: &ScopeEntry) -> (f32, f32) {
        let scale = canvas_width_px / opts.canvas_width_ns as f32;
        let x =
            |ns: NanoSecond| opts.sideways_pan_px + (ns - self.start_ns) as f32 * scale;
        (x(e.start_ns), x(e.end_ns))
    }

    /// Vendor hover hit-test: which scope rect contains the canvas-space
    /// mouse position? Returns the entry index into `self.scopes`.
    pub fn hit_test(
        &self,
        opts: &FlameOptions,
        canvas_width_px: f32,
        mouse: (f32, f32),
    ) -> Option<usize> {
        if opts.canvas_width_ns <= 0.0 {
            return None;
        }
        for (thread_ix, thread) in self.threads.iter().enumerate() {
            if thread.collapsed {
                continue;
            }
            let row_top = thread.scopes_top_y;
            let row_bottom = row_top + thread.max_depth as f32 * (RECT_HEIGHT + SPACING);
            if mouse.1 < row_top || mouse.1 >= row_bottom {
                continue;
            }
            let depth = ((mouse.1 - row_top) / (RECT_HEIGHT + SPACING)) as usize;
            return self
                .scopes
                .iter()
                .position(|e| {
                    e.thread_ix == thread_ix
                        && e.depth == depth
                        && {
                            let (x0, x1) = self.scope_x(opts, canvas_width_px, e);
                            mouse.0 >= x0 && mouse.0 < x1
                        }
                });
        }
        None
    }

    /// Thread whose *label* rect contains the mouse (click toggles collapse).
    pub fn hit_thread_label(&self, mouse: (f32, f32)) -> Option<usize> {
        self.threads.iter().position(|t| {
            let y = t.label_y();
            mouse.1 >= y && mouse.1 < y + TEXT_HEIGHT && mouse.0 <= t.label_width()
        })
    }
}

/// Pre-order walk of a raw scope subtree (vendor `paint_scope` recursion):
/// parent pushed before its children.
fn walk_raw(stream: &Stream, scope: &Scope, depth: usize, thread_ix: usize, out: &mut Vec<ScopeEntry>) {
    let children: Vec<Scope> = Reader::with_offset(stream, scope.child_begin_position)
        .and_then(|r| r.read_top_scopes())
        .unwrap_or_default();
    out.push(ScopeEntry {
        start_ns: scope.record.start_ns,
        end_ns: scope.record.stop_ns(),
        id: scope.id,
        data: scope.record.data.to_owned(),
        depth,
        thread_ix,
        merge_pieces: 0,
        child_count: children.len(),
    });
    for child in &children {
        walk_raw(stream, child, depth + 1, thread_ix, out);
    }
}

/// Pre-order walk of a merged scope subtree (vendor `paint_merge_scope`).
fn walk_merge(merge: &MergeScope, parent_start_ns: NanoSecond, thread_ix: usize, out: &mut Vec<ScopeEntry>) {
    let start_ns = parent_start_ns + merge.relative_start_ns;
    let end_ns = start_ns + merge.duration_per_frame_ns;
    for child in &merge.children {
        walk_merge(child, start_ns, thread_ix, out);
    }
    out.push(ScopeEntry {
        start_ns,
        end_ns,
        id: merge.id,
        data: merge.data.to_string(),
        depth: 0,
        thread_ix,
        merge_pieces: merge.num_pieces,
        child_count: merge.children.len(),
    });
}

// ---------------------------------------------------------------------------
// stats.rs: aggregation + sort
// ---------------------------------------------------------------------------

/// Which column the stats table is sorted by (vendor `SortKey`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SortKey {
    Location,
    FunctionName,
    ScopeName,
    Count,
    Size,
    TotalSelfTime,
    MeanSelfTime,
    MaxSelfTime,
}

/// Vendor `SortOrder`. `rev == true` = descending.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SortOrder {
    pub key: SortKey,
    pub rev: bool,
}

impl Default for SortOrder {
    fn default() -> Self {
        Self {
            key: SortKey::Count,
            rev: true,
        }
    }
}

/// Vendor `ScopeStats`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScopeStats {
    pub count: usize,
    pub bytes: usize,
    /// Time covered by all scopes, minus those covered by child scopes.
    pub total_self_ns: NanoSecond,
    /// Time covered by the slowest scope, minus children.
    pub max_ns: NanoSecond,
}

/// One stats-table row: scope names joined from the `ScopeCollection`
/// plus its aggregated numbers.
#[derive(Clone, Debug)]
pub struct StatRow {
    /// Vendor rows are keyed by `ScopeId`; kept for tooltips/copy parity.
    #[allow(dead_code)]
    pub id: ScopeId,
    /// `ScopeDetails::scope_name` (absent for function scopes → `''`).
    pub name: String,
    pub function_name: String,
    pub location: String,
    pub stats: ScopeStats,
}

/// Recursive self-time aggregation over every stream of every selected
/// frame (vendor `stats::collect_stream`/`collect_scope`).
pub fn collect_scope_stats(frames: &[Arc<UnpackedFrameData>]) -> BTreeMap<ScopeId, ScopeStats> {
    let mut stats = BTreeMap::new();
    for frame in frames {
        for stream_info in frame.thread_streams.values() {
            collect_stream(&mut stats, &stream_info.stream);
        }
    }
    stats
}

fn collect_stream(stats: &mut BTreeMap<ScopeId, ScopeStats>, stream: &Stream) {
    let top_scopes = match Reader::from_start(stream).read_top_scopes() {
        Ok(scopes) => scopes,
        Err(_) => return,
    };
    for scope in top_scopes {
        collect_scope(stats, stream, &scope);
    }
}

fn collect_scope(stats: &mut BTreeMap<ScopeId, ScopeStats>, stream: &Stream, scope: &Scope) {
    let mut ns_used_by_children = 0;
    let children: Vec<Scope> = Reader::with_offset(stream, scope.child_begin_position)
        .and_then(|r| r.read_top_scopes())
        .unwrap_or_default();
    for child in &children {
        collect_scope(stats, stream, child);
        ns_used_by_children += child.record.duration_ns;
    }

    let self_time = scope.record.duration_ns.saturating_sub(ns_used_by_children);
    let entry = stats.entry(scope.id).or_default();
    entry.count += 1;
    entry.bytes += scope_byte_size(scope);
    entry.total_self_ns += self_time;
    entry.max_ns = entry.max_ns.max(self_time);
}

fn scope_byte_size(scope: &Scope) -> usize {
    1 + // `(` sentinel
    8 + // start time
    8 + // scope id
    1 + scope.record.data.len() + // dynamic data len
    8 + // scope size
    1 + // `)` sentinel
    8 // stop time
}

/// Join aggregated stats with `ScopeDetails` names and apply the filter
/// (function name OR scope name match; vendor stats row filter).
pub fn build_stat_rows(
    scope_infos: &ScopeCollection,
    stats: &BTreeMap<ScopeId, ScopeStats>,
    filter: &Filter,
) -> Vec<StatRow> {
    let mut rows: Vec<StatRow> = stats
        .iter()
        .filter_map(|(id, s)| {
            let details = scope_infos.fetch_by_id(id)?;
            let name = details
                .scope_name
                .as_ref()
                .map(|n| n.to_string())
                .unwrap_or_default();
            if !filter.is_empty() && !filter.include(&details.function_name) && !filter.include(&name)
            {
                return None;
            }
            Some(StatRow {
                id: *id,
                name,
                function_name: details.function_name.to_string(),
                location: details.location(),
                stats: *s,
            })
        })
        .collect();
    sort_rows(&mut rows, SortOrder::default());
    rows
}

/// Vendor `SortOrder::sort_scopes`.
pub fn sort_rows(rows: &mut [StatRow], order: SortOrder) {
    match order.key {
        SortKey::Location => rows.sort_by(|a, b| a.location.cmp(&b.location)),
        SortKey::FunctionName => rows.sort_by(|a, b| a.function_name.cmp(&b.function_name)),
        SortKey::ScopeName => rows.sort_by(|a, b| a.name.cmp(&b.name)),
        SortKey::Count => rows.sort_by_key(|r| r.stats.count),
        SortKey::Size => rows.sort_by_key(|r| r.stats.bytes),
        SortKey::TotalSelfTime => rows.sort_by_key(|r| r.stats.total_self_ns),
        SortKey::MeanSelfTime => rows
            .sort_by_key(|r| r.stats.total_self_ns / r.stats.count.max(1) as NanoSecond),
        SortKey::MaxSelfTime => rows.sort_by_key(|r| r.stats.max_ns),
    }
    if order.rev {
        rows.reverse();
    }
}

/// (total bytes, total self ns) caption line above the table.
pub fn stats_totals(stats: &BTreeMap<ScopeId, ScopeStats>) -> (usize, NanoSecond) {
    stats
        .values()
        .fold((0usize, 0i64), |(b, ns), s| (b + s.bytes, ns + s.total_self_ns))
}

// ---------------------------------------------------------------------------
// Shared formatting helpers (vendor flamegraph.rs `to_ms`/`grid_text`)
// ---------------------------------------------------------------------------

pub fn to_ms(ns: NanoSecond) -> f64 {
    ns as f64 * 1e-6
}

/// Vendor `grid_text`: grid-line label.
pub fn grid_text(grid_ns: NanoSecond) -> String {
    let grid_ms = to_ms(grid_ns);
    if grid_ns % 1_000_000 == 0 {
        format!("{grid_ms:.0} ms")
    } else if grid_ns % 100_000 == 0 {
        format!("{grid_ms:.1} ms")
    } else if grid_ns % 10_000 == 0 {
        format!("{grid_ms:.2} ms")
    } else {
        format!("{grid_ms:.3} ms")
    }
}

/// Vendor `color_from_duration` — brighter = more time (blue → green).
pub fn color_from_duration(ns: NanoSecond) -> (f32, f32, f32, f32) {
    let ms = to_ms(ns) as f32;
    let clamp = |v: f32, lo: f32, hi: f32| v.clamp(lo, hi);
    let remap = |v: f32, (a, b): (f32, f32), (c, d): (f32, f32)| {
        let t = clamp((v - a) / (b - a), 0.0, 1.0);
        c + t * (d - c)
    };
    let b = remap(ms, (0.0, 5.0), (1.0, 0.3));
    let r = remap(ms, (0.0, 10.0), (0.5, 0.8));
    let g = remap(ms, (10.0, 33.0), (0.1, 0.8));
    (r, g, b, 0.9)
}

// ---------------------------------------------------------------------------
// Tests: aggregation over synthetic frames (explicit imports, no super::*)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use puffin::{NanoSecond, ScopeCollection, ScopeId, Stream, StreamInfo, ThreadInfo, UnpackedFrameData};
    use std::collections::BTreeMap;
    use std::num::NonZeroU32;
    use std::sync::Arc;

    use crate::puffin_flame::{
        Filter, FlameLayout, FlameOptions, ScopeStats, SortKey, SortOrder, StatRow, SelectedFrames,
        build_stat_rows, collect_scope_stats, color_from_duration, grid_text, sort_rows,
        stats_totals,
    };

    fn scope_id(n: u32) -> ScopeId {
        ScopeId(NonZeroU32::new(n).expect("scope id must be non-zero"))
    }

    fn thread(name: &str) -> ThreadInfo {
        ThreadInfo {
            start_time_ns: Some(0),
            name: name.to_owned(),
        }
    }

    /// Writes one scope (with the given children) into the stream and
    /// returns it, mirroring `puffin::profile_scope!` stream events.
    fn write_scope(
        s: &mut Stream,
        spec: (u32, &str, NanoSecond, NanoSecond),
        children: &[((u32, &str, NanoSecond, NanoSecond), Vec<(u32, &str, NanoSecond, NanoSecond)>)],
    ) -> usize {
        let (off, _) = s.begin_scope(|| spec.2, scope_id(spec.0), spec.1);
        for child in children {
            let (child_off, _) = s.begin_scope(|| child.0 .2, scope_id(child.0 .0), child.0 .1);
            s.end_scope(child_off, child.0 .3);
        }
        s.end_scope(off, spec.3);
        off
    }

    fn frame(idx: u64, thread: &ThreadInfo, stream: StreamInfo) -> Arc<UnpackedFrameData> {
        let mut streams = BTreeMap::new();
        streams.insert(thread.clone(), stream);
        Arc::new(UnpackedFrameData::new(idx, streams).expect("frame"))
    }

    /// `frame_index` frames on one thread, each containing one top scope
    /// (id 1, [t0, t1)) with the listed children.
    fn simple_frames(n: usize) -> Vec<Arc<UnpackedFrameData>> {
        let th = thread("main");
        (0..n)
            .map(|i| {
                let base = (i as NanoSecond) * 1000;
                let mut s = Stream::default();
                write_scope(
                    &mut s,
                    (1, "", base, base + 100),
                    &[((2, "", base + 10, base + 50), vec![])],
                );
                frame(i as u64, &th, StreamInfo::parse(s).expect("parse"))
            })
            .collect()
    }

    #[test]
    fn selected_frames_builds_threads_ranges_and_dedup() {
        let frames = simple_frames(2);
        assert_eq!(frames.len(), 2);
        let dup = vec![frames[0].clone(), frames[0].clone(), frames[1].clone()];
        let sel = SelectedFrames::try_from_iter(&ScopeCollection::default(), dup.into_iter())
            .expect("non-empty");
        assert_eq!(sel.frames.len(), 2, "dedup by frame index");
        assert_eq!(sel.raw_range_ns, (0, 1100));
        assert_eq!(sel.threads.len(), 1);
        let ts = sel.threads.values().next().expect("thread");
        assert_eq!(ts.streams.len(), 2, "one stream per frame");
        assert_eq!(ts.max_depth, 2);
        assert!(sel.contains(0) && sel.contains(1) && !sel.contains(2));
        assert!(
            SelectedFrames::try_from_iter(&ScopeCollection::default(), Vec::new().into_iter())
                .is_none(),
            "empty input is None"
        );
    }

    #[test]
    fn merge_scopes_merges_siblings_by_id_and_data() {
        let th = thread("main");
        let mk = |i: u64, data: &'static str, start: NanoSecond| {
            let mut s = Stream::default();
            write_scope(
                &mut s,
                (7, "", start, start + 100),
                &[
                    (9, data, start + 10, start + 30),
                    (9, data, start + 40, start + 80),
                ]
                    .iter()
                    .map(|&c| (c, vec![]))
                    .collect::<Vec<_>>(),
            );
            frame(i as u64, &th, StreamInfo::parse(s).expect("parse"))
        };
        let frames = vec![mk(0, "a", 1000), mk(1, "a", 2000)];
        let sel = SelectedFrames::try_from_iter(&ScopeCollection::default(), frames.into_iter())
            .expect("frames");

        let merged = &sel.threads.values().next().expect("t").merged_scopes;
        assert_eq!(merged.len(), 1, "same id+data merged across frames");
        let top = &merged[0];
        assert_eq!(top.num_pieces, 2, "one piece per frame");
        assert_eq!(top.duration_per_frame_ns, 100, "total/frames");
        assert_eq!(top.max_duration_ns, 100);

        // Two same-id children per frame merge into one merged child of 4 pieces.
        let children = &top.children;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].num_pieces, 4);
        assert_eq!(children[0].duration_per_frame_ns, 60, "(20+40)*2/2");

        // Different data → different merge node.
        let frames = vec![mk(0, "a", 1000), mk(1, "b", 2000)];
        let sel = SelectedFrames::try_from_iter(&ScopeCollection::default(), frames.into_iter())
            .expect("frames");
        let merged = &sel.threads.values().next().expect("t").merged_scopes;
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].children.len(), 2, "data differs → not merged");
        let mut datas: Vec<&str> = merged[0].children.iter().map(|c| c.data.as_ref()).collect();
        datas.sort_unstable();
        assert_eq!(datas, ["a", "b"]);
    }

    #[test]
    fn stats_self_time_excludes_children() {
        let frames = simple_frames(1);
        let stats = collect_scope_stats(&frames);
        assert_eq!(stats.len(), 2);

        let top = stats[&scope_id(1)];
        assert_eq!(top.count, 1);
        assert_eq!(top.total_self_ns, 60, "100 - 40 child");
        assert_eq!(top.max_ns, 60);

        let child = stats[&scope_id(2)];
        assert_eq!(child.count, 1);
        assert_eq!(child.total_self_ns, 40);
        // 1 + 8 + 8 + 1 + 0 + 8 + 1 + 8 = 35 bytes per scope occurrence.
        assert_eq!(child.bytes, 35);
    }

    #[test]
    fn stats_aggregate_across_frames() {
        let frames = simple_frames(3);
        let stats = collect_scope_stats(&frames);
        assert_eq!(stats[&scope_id(1)].count, 3);
        assert_eq!(stats[&scope_id(1)].total_self_ns, 180);
        let (bytes, ns) = stats_totals(&stats);
        assert_eq!(bytes, 3 * 70);
        assert_eq!(ns, 180 + 120);
    }

    #[test]
    fn filter_is_case_insensitive_substring() {
        let mut f = Filter::default();
        assert!(f.is_empty() && f.include("Anything"));
        f.set("Mesh");
        assert!(f.include("load_mesh"));
        assert!(f.include("LOAD_MESH_X"));
        assert!(!f.include("draw"));
    }

    #[test]
    fn stat_rows_sort_by_key_and_direction() {
        let mk = |name: &str, func: &str, count: usize, ns: NanoSecond| StatRow {
            id: scope_id(1),
            name: name.to_owned(),
            function_name: func.to_owned(),
            location: String::new(),
            stats: ScopeStats {
                count,
                bytes: count * 10,
                total_self_ns: ns,
                max_ns: ns,
            },
        };
        let mut rows = vec![mk("a", "fa", 2, 100), mk("b", "fb", 9, 300), mk("c", "fc", 5, 200)];

        sort_rows(&mut rows, SortOrder { key: SortKey::Count, rev: true });
        assert_eq!(rows[0].function_name, "fb");
        assert_eq!(rows[2].function_name, "fa");

        sort_rows(&mut rows, SortOrder { key: SortKey::Count, rev: false });
        assert_eq!(rows[0].function_name, "fa");

        sort_rows(&mut rows, SortOrder { key: SortKey::FunctionName, rev: false });
        assert_eq!(rows[0].function_name, "fa");
        assert_eq!(rows[2].function_name, "fc");
    }

    #[test]
    fn layout_height_and_hit_test() {
        let frames = simple_frames(1);
        let sel =
            SelectedFrames::try_from_iter(&ScopeCollection::default(), frames.into_iter())
                .expect("frames");
        let opts = FlameOptions::default();
        let layout = FlameLayout::build(&sel, &opts);

        // Timeline row + separator(4) + label row(15) + 2 depth rows(2*20) + inter-thread(15).
        let expected = 15.0 + 4.0 + 15.0 + 2.0 * (16.0 + 4.0) + 15.0;
        assert_eq!(layout.height, expected);
        assert_eq!(layout.scopes.len(), 2);
        assert_eq!(layout.scopes[0].depth, 0, "pre-order: parent first");
        assert_eq!(layout.scopes[0].child_count, 1);

        // Zoom = full range across `canvas_w` px: frame range is [0,100)ns,
        // so reset_view yields 100 ns across 100 px → 1 ns per px.
        let canvas_w = 100.0_f32;
        let mut opts = FlameOptions::default();
        opts.reset_view(sel.raw_range_ns);
        assert_eq!(opts.canvas_width_ns, 100.0);

        // Top scope [0,100)ns sits at [0,100)px; child at [10,50)px.
        let hit_top = layout.hit_test(&opts, canvas_w, (30.0, layout.threads[0].scopes_top_y + 2.0));
        assert_eq!(hit_top, Some(0));
        let hit_child =
            layout.hit_test(&opts, canvas_w, (20.0, layout.threads[0].scopes_top_y + 20.0 + 2.0));
        assert_eq!(hit_child, Some(1));
        let e = &layout.scopes[0];
        assert_eq!(
            ((e.start_ns - layout.start_ns) as f32, (e.end_ns - layout.start_ns) as f32),
            (0.0, 100.0)
        );

        // Outside the child's x range on its row → None.
        assert_eq!(
            layout.hit_test(&opts, canvas_w, (60.0, layout.threads[0].scopes_top_y + 20.0 + 2.0)),
            None
        );
        // Below the thread block → None.
        assert_eq!(layout.hit_test(&opts, canvas_w, (30.0, layout.height + 100.0)), None);

        // Thread label click zone.
        assert_eq!(layout.hit_thread_label((10.0, layout.threads[0].label_y() + 2.0)), Some(0));
        assert_eq!(layout.hit_thread_label((10.0, layout.threads[0].scopes_top_y + 2.0)), None);
    }

    #[test]
    fn zoom_pan_and_formatting_helpers() {
        let mut opts = FlameOptions::default();
        // vendor click-zoom: range [50,150) relative to start 0 → pan = -canvas*50/100
        opts.zoom_to((50, 150), 200.0, 0);
        assert_eq!(opts.canvas_width_ns, 100.0);
        assert_eq!(opts.sideways_pan_px, -100.0);

        opts.reset_view((0, 4000));
        assert_eq!(opts.canvas_width_ns, 4000.0);
        assert_eq!(opts.sideways_pan_px, 0.0);

        assert_eq!(grid_text(1_000_000), "1 ms");
        assert_eq!(grid_text(150_000), "0.15 ms");
        assert_eq!(grid_text(12_345), "0.012 ms");
        // 1ms-ish is greenish/brighter than 0.1ms-ish blue.
        let (r0, g0, b0, _) = color_from_duration(100_000);
        let (r1, g1, b1, _) = color_from_duration(33_000_000);
        assert!(b0 > b1 && g1 > g0 && r1 > r0);
    }
}
