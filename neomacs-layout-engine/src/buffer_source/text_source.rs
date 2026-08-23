use crate::buffer_source::producer::frame::{
    DisplayReplacementExtentLookup, ReplacementCoveredSpan,
};
use crate::display_item::{
    BufferDisplayPropertyReplacementItem, BufferDisplayReplacementSource, DisplayItem,
    DisplayItemKind, DisplayItemLayout, DisplayLineHeightPolicy, DisplayPointerAppearance,
    DisplayPointerSourceRange, DisplaySourceMappedText, DisplaySourcePosition, DisplayTextRun,
    RenderFaceRef, SourceSpan,
};
use crate::display_property::DisplayPropertyClassification;
use crate::display_source::{
    DisplayItemSource, DisplayPropertySourceCursorAction, DisplayPropertySourceFaces,
    DisplayPropertySourcePlan, DisplaySourceContext, LispStringSourceStack,
    TextSourceCharClassification, classify_text_source_char,
    display_item_kind_for_text_source_char,
};
use crate::neovm_bridge::{
    LayoutBufferView, LayoutCharPropertyLookup, OrderedFaceSources, OverlayDisplayString,
    RustTextPropAccess,
};
use crate::unicode::decode_utf8;
use neovm_core::buffer::{BufferId, CharLen, CharPos0, EmacsBytePos, EmacsByteRange};
use neovm_core::emacs_core::Value;
use neovm_core::emacs_core::composite::composition_display_text_for_property;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(test)]
static EXACT_MOUSE_FACE_EXTENT_QUERY_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn reset_exact_mouse_face_extent_query_count() {
    EXACT_MOUSE_FACE_EXTENT_QUERY_COUNT.store(0, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn exact_mouse_face_extent_query_count() -> usize {
    EXACT_MOUSE_FACE_EXTENT_QUERY_COUNT.load(Ordering::Relaxed)
}

fn record_exact_mouse_face_extent_query() {
    #[cfg(test)]
    EXACT_MOUSE_FACE_EXTENT_QUERY_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BufferTextDisplayReplacementMode {
    InlineSourceItems,
    TypedReplacementItem,
}

impl BufferTextDisplayReplacementMode {
    pub(crate) fn consumes_typed_replacements(self) -> bool {
        matches!(self, Self::TypedReplacementItem)
    }

    fn inlines_replacement_strings(self) -> bool {
        matches!(self, Self::InlineSourceItems)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BufferTextCursorItem {
    Item(DisplayItem),
    DisplayPropertyReplacement(BufferDisplayPropertyReplacementItem),
    /// Overlay before/after-strings anchored at a buffer position, collected and
    /// GNU-ordered by the producer. INSERTION semantics (GNU `push_it (it,
    /// NULL)`): the buffer position does not advance, so the character at the
    /// anchor is produced next and the strings render in front of it.
    OverlayStrings(BufferOverlayStringsItem),
}

/// The overlay strings anchored at one buffer position, in GNU
/// `compare_overlay_entries` order.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BufferOverlayStringsItem {
    anchor_charpos: CharPos0,
    strings: Vec<OverlayDisplayString>,
}

/// The first and last characters in one non-empty, GNU-ordered overlay-string
/// element.  Word wrapping checks the first character before append and uses
/// the last character to determine whether the following buffer character may
/// open another wrap candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BufferOverlayStringWordWrapBoundary {
    first: char,
    last: char,
}

impl BufferOverlayStringWordWrapBoundary {
    pub(crate) const fn first(self) -> char {
        self.first
    }

    pub(crate) const fn last(self) -> char {
        self.last
    }
}

impl BufferOverlayStringsItem {
    pub(crate) fn anchor_charpos(&self) -> CharPos0 {
        self.anchor_charpos
    }

    pub(crate) fn strings(&self) -> &[OverlayDisplayString] {
        &self.strings
    }

    /// Decode the boundary characters of the strings exactly as the display
    /// source does.  Collection rejects empty strings, but `Option` keeps this
    /// boundary total if another producer is added later.
    pub(crate) fn word_wrap_boundary(&self) -> Option<BufferOverlayStringWordWrapBoundary> {
        let mut first = None;
        let mut last = None;
        for entry in &self.strings {
            let string = entry.string.as_lisp_string()?;
            let bytes = string.as_bytes();
            let mut offset = 0;
            while offset < bytes.len() {
                let (ch, len) = decode_utf8(&bytes[offset..]);
                if len == 0 {
                    break;
                }
                first.get_or_insert(ch);
                last = Some(ch);
                offset = offset.saturating_add(len);
            }
        }
        Some(BufferOverlayStringWordWrapBoundary {
            first: first?,
            last: last?,
        })
    }
}

/// A `DisplayItemSource` that reads plain buffer text (with face and display
/// property boundaries) and emits `DisplayItem` values for the shared row
/// renderer. The main buffer walk consumes this cursor through the source-walk
/// bridge, which preserves typed display items while splitting text runs only
/// where the remaining buffer walk still needs per-character wrap/cursor
/// decisions.
pub(crate) struct BufferTextSourceCursor<'a, B: LayoutBufferView + ?Sized> {
    buffer_id: BufferId,
    buffer: &'a B,
    /// The window this cursor lays out for, so overlay `window` properties are
    /// honored (`overlay_applies_to_window`): faces, `display`, and `mouse-face`
    /// contributed by a windowed overlay (e.g. hl-line non-sticky) apply only in
    /// that window. `None` for non-window contexts (unrestricted, matching GNU).
    window_id: Option<u64>,
    char_pos: CharPos0,
    end: CharPos0,
    /// While the cursor sits before this position, plain text runs are produced
    /// ONE CHARACTER AT A TIME.
    ///
    /// The multi-character `TextRun` is a batching optimization over GNU's
    /// strictly one-element-at-a-time producer; the renderer declines it for the
    /// rest of a run it has already refused, because it will render that run
    /// character by character anyway (overflow, cursor capture and the word-wrap
    /// candidate hook are all per character). Without this the renderer re-reads
    /// — and re-measures — the whole remaining run once per character: measured
    /// at 79x the run measurements and 77x the measured characters on a
    /// 2000-character wrapped line.
    ///
    /// It is a HINT, not state the output depends on: losing it costs run
    /// batching and nothing else, which is what separates it from the pending
    /// queue it replaces. It expires by position, so a skip past `end` (the
    /// truncation or invisible skip) self-heals.
    char_granularity_end: Option<CharPos0>,
    /// The position whose overlay strings have already been produced.
    ///
    /// The overlay-strings element carries INSERTION semantics, so producing it
    /// does not advance the cursor: without this marker the same anchor would be
    /// surfaced again on the very next production and the strings would repeat
    /// forever. It is the minimal form of GNU's `push_it (it, NULL)` frame —
    /// pushed at a position, popped when its content is exhausted, resuming at
    /// exactly the position it was pushed at.
    ///
    /// A word-wrap retry clears this marker when its checkpoint precedes the
    /// string, replaying the complete insertion element like GNU RESTORE_IT.
    /// A character-wrap retry preserves it because the already-drawn string is
    /// not part of the overflowing buffer character.
    overlay_strings_produced_at: Option<CharPos0>,
    base_face: RenderFaceRef,
    replacement_strings: LispStringSourceStack,
    mouse_face_extent: Option<CachedMouseFaceExtent>,
    face_property: LayoutCharPropertyLookup,
    display_property: LayoutCharPropertyLookup,
    line_height_property: LayoutCharPropertyLookup,
}

#[derive(Clone, Copy)]
struct NonNilMouseFace(Value);

impl NonNilMouseFace {
    fn new(value: Value) -> Option<Self> {
        (!value.is_nil()).then_some(Self(value))
    }

    fn value(self) -> Value {
        self.0
    }
}

#[derive(Clone, Copy)]
enum MouseFaceOwner {
    Text,
    Overlay(Value),
}

impl MouseFaceOwner {
    fn overlay(self) -> Option<Value> {
        match self {
            Self::Text => None,
            Self::Overlay(overlay) => Some(overlay),
        }
    }
}

#[derive(Clone, Copy)]
struct ResolvedMouseFace {
    value: NonNilMouseFace,
    range: EmacsByteRange,
    owner: MouseFaceOwner,
}

#[derive(Clone, Copy)]
enum CachedMouseFaceExtent {
    Absent { range: EmacsByteRange },
    Active(ResolvedMouseFace),
}

impl CachedMouseFaceExtent {
    fn range(self) -> EmacsByteRange {
        match self {
            Self::Absent { range } | Self::Active(ResolvedMouseFace { range, .. }) => range,
        }
    }

    fn contains(self, bytepos: EmacsBytePos) -> bool {
        let range = self.range();
        range.start() <= bytepos && bytepos < range.end()
    }

    fn resolved(self) -> Option<ResolvedMouseFace> {
        match self {
            Self::Absent { .. } => None,
            Self::Active(resolved) => Some(resolved),
        }
    }
}

impl<'a, B: LayoutBufferView + ?Sized> BufferTextSourceCursor<'a, B> {
    /// Cursor with no window context (overlay `window` properties unrestricted).
    /// Used by tests and any non-window caller; the redisplay path uses
    /// [`new_for_window`](Self::new_for_window).
    #[allow(dead_code)] // retained for non-window callers and focused tests
    pub(crate) fn new(
        buffer_id: BufferId,
        buffer: &'a B,
        start: CharPos0,
        end: CharPos0,
        base_face: RenderFaceRef,
    ) -> Self {
        Self::new_for_window(buffer_id, buffer, None, start, end, base_face)
    }

    /// Cursor scoped to `window_id`, so overlay `window` properties are honored
    /// (a windowed overlay's face / `display` / `mouse-face` applies only there).
    pub(crate) fn new_for_window(
        buffer_id: BufferId,
        buffer: &'a B,
        window_id: Option<u64>,
        start: CharPos0,
        end: CharPos0,
        base_face: RenderFaceRef,
    ) -> Self {
        let accessible_end = buffer.layout_point_max_char_pos();
        let start = start.min(accessible_end);
        let end = end.min(accessible_end).max(start);
        Self {
            buffer_id,
            buffer,
            window_id,
            char_pos: start,
            end,
            char_granularity_end: None,
            overlay_strings_produced_at: None,
            base_face,
            replacement_strings: LispStringSourceStack::empty(1),
            mouse_face_extent: None,
            face_property: LayoutCharPropertyLookup::new(buffer, Value::symbol("face")),
            display_property: LayoutCharPropertyLookup::new(buffer, Value::symbol("display")),
            line_height_property: LayoutCharPropertyLookup::new(
                buffer,
                Value::symbol("line-height"),
            ),
        }
    }

    pub(crate) fn current_char_pos(&self) -> CharPos0 {
        self.char_pos
    }

    /// The overlay strings anchored at `char_pos`, GNU-ordered and filtered to
    /// this cursor's window, or `None` when the position anchors none.
    ///
    /// GNU collects overlay strings in `handle_stop` before the buffer character
    /// at the same position, which is why this runs ahead of every other handler
    /// in [`next_cursor_item`](Self::next_cursor_item).
    fn overlay_strings_at(&self, char_pos: CharPos0) -> Option<Vec<OverlayDisplayString>> {
        if self.buffer.layout_overlays().is_empty() {
            return None;
        }
        let strings = RustTextPropAccess::new_for_optional_window(self.buffer, self.window_id)
            .overlay_strings_at(char_pos.get() as i64);
        (!strings.is_empty()).then_some(strings)
    }

    /// A produced run must never CROSS an overlay-string anchor: anchors are
    /// surfaced as their own element at the position they anchor, and the
    /// producer only looks for them at a run's START, so a run that swallowed
    /// one would drop its strings entirely.
    ///
    /// The invariant holds structurally — `next_property_change` bounds every
    /// run at every overlay boundary (this engine's compute_stop_pos) and an
    /// anchor IS an overlay boundary — so this is the debug-build statement of
    /// the property the frame entry relies on. It moved here from the renderer's
    /// whole-run guard when P4.6 made the producer emit the strings.
    fn debug_assert_no_overlay_string_anchor_inside(&self, start: CharPos0, end: CharPos0) {
        if !cfg!(debug_assertions) || self.buffer.layout_overlays().is_empty() {
            return;
        }
        let text_props = RustTextPropAccess::new_for_optional_window(self.buffer, self.window_id);
        let mut charpos = start.get() as i64;
        let end_charpos = end.get() as i64;
        while let Some(next) = text_props.next_overlay_boundary_charpos_after(charpos) {
            if next <= charpos || next >= end_charpos {
                return;
            }
            debug_assert!(
                text_props.overlay_strings_at(next).is_empty(),
                "a produced run crossed the overlay-string anchor at {next} \
                 (run {start:?}..{end:?}); the anchor's strings would never be \
                 emitted"
            );
            charpos = next;
        }
    }

    /// Decline run batching until `end_charpos`: see
    /// [`char_granularity_end`](Self::char_granularity_end).
    pub(crate) fn request_char_granularity_until(&mut self, end_charpos: CharPos0) {
        self.char_granularity_end = Some(match self.char_granularity_end {
            Some(current) => current.max(end_charpos),
            None => end_charpos,
        });
    }

    /// The live batching-decline extent, for the producer's snapshot.
    pub(crate) fn char_granularity_end(&self) -> Option<CharPos0> {
        self.char_granularity_end
    }

    /// Reinstate (or drop) the batching decline — used by the producer's
    /// snapshot/restore and by the row-transition retry, which resumes with runs
    /// batched again because the continuation row re-measures from a fresh pen.
    pub(crate) fn set_char_granularity_end(&mut self, end_charpos: Option<CharPos0>) {
        self.char_granularity_end = end_charpos;
    }

    fn produces_single_chars_at(&self, char_pos: CharPos0) -> bool {
        self.char_granularity_end.is_some_and(|end| char_pos < end)
    }

    pub(crate) fn reset_to(&mut self, char_pos: CharPos0) {
        let accessible_end = self.buffer.layout_point_max_char_pos();
        self.char_pos = char_pos.min(self.end).min(accessible_end);
    }

    /// Restore a word-wrap checkpoint.  Any insertion element produced at or
    /// after the checkpoint was also removed by the glyph checkpoint and must
    /// be surfaced again on the continuation row.
    pub(crate) fn rewind_for_word_wrap_to(&mut self, char_pos: CharPos0) {
        self.reset_to(char_pos);
        if self
            .overlay_strings_produced_at
            .is_some_and(|anchor| anchor >= self.char_pos)
        {
            self.overlay_strings_produced_at = None;
        }
    }

    fn byte_pos(&self, char_pos: CharPos0) -> EmacsBytePos {
        self.buffer.layout_char_pos_to_emacs_byte_pos(char_pos)
    }

    pub(crate) fn char_at(&self, char_pos: CharPos0) -> Option<char> {
        if char_pos >= self.end {
            return None;
        }
        let start = self.byte_pos(char_pos);
        let end = self.byte_pos(char_pos.add_len(CharLen::new(1)).min(self.end));
        let mut bytes = Vec::new();
        self.buffer
            .layout_copy_emacs_byte_range_to(EmacsByteRange::new(start, end), &mut bytes);
        let (ch, len) = decode_utf8(&bytes);
        (len > 0).then_some(ch)
    }

    fn text_slice(&self, start: CharPos0, end: CharPos0) -> String {
        let mut bytes = Vec::new();
        self.buffer.layout_copy_emacs_byte_range_to(
            EmacsByteRange::new(self.byte_pos(start), self.byte_pos(end)),
            &mut bytes,
        );
        let mut text = String::new();
        let mut offset = 0usize;
        while offset < bytes.len() {
            let (ch, len) = decode_utf8(&bytes[offset..]);
            if len == 0 {
                break;
            }
            text.push(ch);
            offset += len;
        }
        text
    }

    fn span(&self, start: CharPos0, end: CharPos0) -> SourceSpan {
        SourceSpan::new(
            DisplaySourcePosition::buffer(self.buffer_id, start, self.byte_pos(start)),
            DisplaySourcePosition::buffer(self.buffer_id, end, self.byte_pos(end)),
        )
    }

    /// neomacs equivalent of GNU `compute_stop_pos`: the next position at which
    /// the face/display can change, i.e. where the current text run must end.
    /// GNU folds the next text-property change AND the next *overlay* change
    /// (`next_overlay_change`) into `it->stop_charpos` (src/xdisp.c
    /// compute_stop_pos). We mirror both: without the overlay-change bound, a
    /// face-only overlay (isearch current-match, lazy-highlight, region) that
    /// begins or ends *inside* a text-property run would not split the run, so
    /// the run would keep the face resolved at its start and the overlay would
    /// never paint (or would paint over the whole run).
    fn next_property_change(&self, char_pos: CharPos0) -> CharPos0 {
        let byte = self.byte_pos(char_pos);
        let prop_change = self
            .buffer
            .layout_next_text_prop_change_after_emacs_byte_pos(byte)
            .map(|byte_pos| self.buffer.layout_emacs_byte_pos_to_char_pos(byte_pos))
            .unwrap_or(self.end);
        let overlay_change = self
            .buffer
            .layout_overlays()
            .next_boundary_after_emacs_byte_pos(byte)
            .map(|byte_pos| self.buffer.layout_emacs_byte_pos_to_char_pos(byte_pos))
            .unwrap_or(self.end);
        prop_change.min(overlay_change).min(self.end)
    }

    fn display_prop_at(&self, char_pos: CharPos0) -> Option<Value> {
        // GNU `get_char_property` (src/textprop.c): an overlay `display` property
        // overrides the text property (highest-priority overlay wins). Several
        // common features attach `display` to an OVERLAY covering a region rather
        // than to a text property — notably `org-display-inline-images`, which
        // overlays the link with `(image …)`. Reading only the text property here
        // left those rendered as raw text. The run is already bounded at every
        // overlay boundary by `compute_stop_pos`, so the replacement spans the
        // overlay's extent.
        // The single-winner policy, from the shared core primitive: the
        // highest-precedence window-visible overlay carrying `display` wins and
        // shadows the text property. Ordering is GNU `compare_overlays` (priority,
        // containment, secondary priority, stable tiebreak); the previous
        // `max_by_key` on a bare `priority` integer read a `(PRIMARY . SECONDARY)`
        // priority as 0, ignored containment, and broke ties arbitrarily.
        self.display_prop_source_at(char_pos)
            .map(|source| source.value)
    }

    /// The winning `display` value AND its source, which decides how far the
    /// replacement reaches — see [`ReplacementCoveredSpan::for_property_source`].
    fn display_prop_source_at(
        &self,
        char_pos: CharPos0,
    ) -> Option<crate::neovm_bridge::CharPropertySource> {
        self.display_property.overlay_or_text_source_at(
            self.buffer,
            self.byte_pos(char_pos),
            self.window_id,
        )
    }

    fn composition_prop_at(&self, char_pos: CharPos0) -> Option<Value> {
        self.buffer.layout_text_prop_at_emacs_byte_pos(
            self.byte_pos(char_pos),
            Value::symbol("composition"),
        )
    }

    fn display_replacement_source(
        &self,
        start: CharPos0,
        end: CharPos0,
    ) -> BufferDisplayReplacementSource {
        BufferDisplayReplacementSource::spanning(
            self.buffer_id,
            start,
            self.byte_pos(start),
            end,
            self.byte_pos(end),
        )
    }

    fn display_replacement_item(
        &self,
        value: Value,
        classification: DisplayPropertyClassification,
        covered: ReplacementCoveredSpan,
    ) -> BufferDisplayPropertyReplacementItem {
        let (start, end) = (covered.start(), covered.resume());
        BufferDisplayPropertyReplacementItem::new(
            value,
            classification,
            self.display_replacement_source(start, end),
            self.byte_pos(start),
            self.byte_pos(end),
            covered,
        )
    }

    /// Resolve the face for the run beginning at `char_pos`, mirroring GNU
    /// `face_at_buffer_position` (src/xfaces.c): merge the `face` text property
    /// (with `font-lock-face` fallback) FIRST, then overlay faces in ascending
    /// priority order (higher priority wins). Resolving overlay faces here — not
    /// only into the row's base face (which is resolved once at column 0) — is
    /// what lets a face-only overlay (isearch current-match, lazy-highlight,
    /// region) that begins mid-run actually paint: the run is already bounded at
    /// every overlay boundary by `next_property_change` (our compute_stop_pos),
    /// so each piece carries its own overlay-merged face into the glyph.
    fn face_at(&self, char_pos: CharPos0, context: &mut DisplaySourceContext<'_>) -> RenderFaceRef {
        let bytepos = self.byte_pos(char_pos);
        let text_face = self.face_property.text_value_at(self.buffer, bytepos);
        // Overlay faces come from the shared ascending-priority collector.
        // Preserve the complete logical source stack so the resolver merges
        // every lface attribute before realizing inverse-video once.
        let overlays =
            crate::neovm_bridge::overlay_faces_at(self.buffer, bytepos, self.window_id).faces;
        let sources = OrderedFaceSources::from_text_and_overlays(text_face, overlays);
        context.resolve_face_sources(self.base_face, &sources)
    }

    /// GNU `get_char_property_and_overlay` semantics for `mouse-face`: the
    /// highest-priority overlay which supplies a non-nil property wins over
    /// the buffer text property.  Run production is already stopped at every
    /// overlay boundary, so the selected value is stable for this item.
    fn mouse_face_at(
        &mut self,
        char_pos: CharPos0,
        stable_until: CharPos0,
    ) -> Option<ResolvedMouseFace> {
        let bytepos = self.byte_pos(char_pos);
        if let Some(cached) = self
            .mouse_face_extent
            .filter(|cached| cached.contains(bytepos))
        {
            return cached.resolved();
        }

        let overlays = self.buffer.layout_overlays();
        let property = Value::symbol("mouse-face");
        let bounds = EmacsByteRange::new(
            self.buffer.layout_point_min_emacs_byte_pos(),
            self.buffer.layout_point_max_emacs_byte_pos(),
        );
        // A windowed overlay's `mouse-face` applies only in its own window
        // (same `window`-property rule as its face / display / invisible).
        let overlay_extent = overlays.highest_priority_overlay_property_extent_at_emacs_byte_pos(
            bytepos,
            property,
            bounds,
            self.window_id,
        )?;
        if let Some(overlay) = overlay_extent.overlay() {
            let value = NonNilMouseFace::new(overlay_extent.value())
                .expect("overlay mouse-face lookup returns only non-nil values");
            let cached = CachedMouseFaceExtent::Active(ResolvedMouseFace {
                value,
                range: overlay_extent.range(),
                owner: MouseFaceOwner::Overlay(overlay),
            });
            self.mouse_face_extent = Some(cached);
            return cached.resolved();
        }

        let value = self
            .buffer
            .layout_text_prop_at_emacs_byte_pos(bytepos, property)
            .and_then(NonNilMouseFace::new);
        let Some(value) = value else {
            // `stable_until` is the next boundary of ANY text property or
            // overlay.  Therefore a missing mouse-face cannot become active
            // before it, and no exact whole-buffer extent query is needed.
            let stable_end = self
                .byte_pos(stable_until)
                .min(overlay_extent.range().end());
            let cached = CachedMouseFaceExtent::Absent {
                range: EmacsByteRange::new(bytepos, stable_end),
            };
            self.mouse_face_extent = Some(cached);
            return None;
        };

        record_exact_mouse_face_extent_query();
        let text_start = self
            .buffer
            .layout_previous_single_text_prop_change_before_emacs_byte_pos(bytepos, property)
            .unwrap_or_else(|| self.buffer.layout_point_min_emacs_byte_pos());
        let text_end = self
            .buffer
            .layout_next_single_text_prop_change_after_emacs_byte_pos(bytepos, property)
            .unwrap_or_else(|| self.buffer.layout_point_max_emacs_byte_pos());
        let cached = CachedMouseFaceExtent::Active(ResolvedMouseFace {
            value,
            range: EmacsByteRange::new(
                text_start.max(overlay_extent.range().start()),
                text_end.min(overlay_extent.range().end()),
            ),
            owner: MouseFaceOwner::Text,
        });
        self.mouse_face_extent = Some(cached);
        cached.resolved()
    }

    fn pointer_appearance_at(
        &mut self,
        start: CharPos0,
        property_end: CharPos0,
        face: RenderFaceRef,
        context: &mut DisplaySourceContext<'_>,
    ) -> Option<DisplayPointerAppearance> {
        let mouse_face = self.mouse_face_at(start, property_end)?;
        let pointer_face = context.resolve_pointer_face_ref(face, mouse_face.value.value())?;
        let source = DisplayPointerSourceRange::effective(
            DisplaySourcePosition::buffer(
                self.buffer_id,
                CharPos0::ZERO,
                self.byte_pos(CharPos0::ZERO),
            ),
            self.buffer
                .layout_emacs_byte_pos_to_char_pos(mouse_face.range.start())
                .get(),
            self.buffer
                .layout_emacs_byte_pos_to_char_pos(mouse_face.range.end())
                .get(),
            mouse_face.owner.overlay(),
        );
        Some(DisplayPointerAppearance::new(source, pointer_face))
    }

    fn next_text_run_end(&self, start: CharPos0, limit: CharPos0) -> CharPos0 {
        let mut end = start;
        while end < limit {
            let Some(ch) = self.char_at(end) else {
                break;
            };
            if classify_text_source_char(ch) != TextSourceCharClassification::Text {
                break;
            }
            // A char that the active display table remaps to a glyph vector must
            // break the plain text run so it is emitted as its own item (GNU
            // `DISP_CHAR_VECTOR` is consulted per character in the producer).
            if crate::neovm_bridge::buffer_display_table_glyph_vector_p(self.buffer, ch) {
                break;
            }
            end = end.add_len(CharLen::new(1));
        }
        end.max(start.add_len(CharLen::new(1))).min(limit)
    }

    /// Resolve the active display table's glyph vector for `ch`, returning the
    /// decoded glyph characters and any homogeneous face to display in place of `ch` (GNU
    /// `DISP_CHAR_VECTOR`).  `None` is the hot path (no table / no entry / not a
    /// vector) and leaves `ch` to render literally.
    fn display_table_glyphs(
        &self,
        ch: char,
    ) -> Option<crate::neovm_bridge::BufferDisplayTableGlyphs> {
        crate::neovm_bridge::buffer_display_table_glyphs(self.buffer, ch)
    }

    fn display_property_cursor_action(
        &self,
        context: &mut DisplaySourceContext<'_>,
        display_property: &DisplayPropertySourcePlan,
        face: RenderFaceRef,
        span: SourceSpan,
    ) -> DisplayPropertySourceCursorAction {
        display_property.cursor_action(
            context,
            span,
            DisplayPropertySourceFaces::Buffer { effective: face },
        )
    }

    fn push_display_replacement_string(
        &mut self,
        value: Value,
        base_face: RenderFaceRef,
        start: CharPos0,
        end: CharPos0,
    ) {
        self.replacement_strings.push_with_replacement_source(
            value,
            base_face,
            Some(self.display_replacement_source(start, end)),
        );
    }

    fn next_text_item_with_layout(
        &mut self,
        start: CharPos0,
        property_end: CharPos0,
        face: RenderFaceRef,
        layout: DisplayItemLayout,
    ) -> Option<DisplayItem> {
        let ch = self.char_at(start)?;

        if let Some(composition) = self
            .composition_prop_at(start)
            .and_then(composition_display_text_for_property)
        {
            let end = start.add_len(CharLen::new(composition.char_len()));
            if end <= property_end && end <= self.end {
                self.char_pos = end;
                return Some(
                    DisplayItem::new(
                        self.span(start, end),
                        face,
                        DisplayItemKind::SourceMappedText(DisplaySourceMappedText::new(
                            composition.text().to_owned(),
                        )),
                    )
                    .with_layout(layout),
                );
            }
        }

        // GNU `get_next_display_element`: before the control-char / glyphless /
        // tab arms, consult the active display table.  If the char remaps to a
        // glyph vector, the WHOLE vector is ONE display element spanning the
        // single source char — emit it as a `SourceMappedText` over
        // `[start, start+1)`.  The source walk advances by the item's span-end
        // (the single char) exactly once, so byte/charpos never desync; each
        // decoded glyph is appended as a real glyph (a `?\t` glyph re-expands
        // through the ordinary tab path), keeping the row non-blank.
        if let Some(glyphs) = self.display_table_glyphs(ch) {
            let crate::neovm_bridge::BufferDisplayTableGlyphs { text, face_name } = glyphs;
            self.char_pos = start.add_len(CharLen::new(1));
            // GNU iterates a display-table entry element-by-element and decides
            // end-of-line on the DISPLAYED char: `ITERATOR_AT_END_OF_LINE_P`
            // (dispextern.h) tests `it->c == '\n'`, and `next_element_from_
            // display_vector` (xdisp.c) sets `it->c` per glyph. So a newline whose
            // entry ends in a `\n` glyph (whitespace-mode's `[$ \n]`) renders its
            // leading glyphs and THEN ends the row — the trailing `\n` glyph is
            // its own end-of-line element. Emit the leading glyphs and mark the
            // item to break the row after; the buffer newline is consumed here, so
            // the next row resumes on the following char. An entry WITHOUT a
            // trailing `\n` (e.g. `[$]`) keeps the plain no-break replacement,
            // matching GNU (the newline's break is fully replaced -> lines join).
            if ch == '\n' && text.ends_with('\n') {
                let prefix = text[..text.len() - '\n'.len_utf8()].to_string();
                return Some(
                    DisplayItem::new(
                        self.span(start, self.char_pos),
                        face,
                        DisplayItemKind::SourceMappedText(
                            DisplaySourceMappedText::new(prefix).with_face_name(face_name),
                        ),
                    )
                    .with_layout(layout)
                    .with_break_after_row(),
                );
            }
            return Some(
                DisplayItem::new(
                    self.span(start, self.char_pos),
                    face,
                    DisplayItemKind::SourceMappedText(
                        DisplaySourceMappedText::new(text).with_face_name(face_name),
                    ),
                )
                .with_layout(layout),
            );
        }

        if let Some(mut kind) = display_item_kind_for_text_source_char(ch) {
            if let DisplayItemKind::RowBreak(row_break) = &mut kind {
                *row_break = row_break.with_line_height(DisplayLineHeightPolicy::from_property(
                    self.line_height_property.overlay_or_text_value_at(
                        self.buffer,
                        self.byte_pos(start),
                        self.window_id,
                    ),
                ));
            }
            self.char_pos = start.add_len(CharLen::new(1));
            return Some(
                DisplayItem::new(self.span(start, self.char_pos), face, kind).with_layout(layout),
            );
        }

        let end = if self.produces_single_chars_at(start) {
            start.add_len(CharLen::new(1)).min(property_end)
        } else {
            self.next_text_run_end(start, property_end)
        };
        self.debug_assert_no_overlay_string_anchor_inside(start, end);
        self.char_pos = end;
        Some(
            DisplayItem::new(
                self.span(start, end),
                face,
                DisplayItemKind::TextRun(DisplayTextRun::new(self.text_slice(start, end))),
            )
            .with_layout(layout),
        )
    }

    fn next_text_item(
        &mut self,
        start: CharPos0,
        property_end: CharPos0,
        face: RenderFaceRef,
    ) -> Option<DisplayItem> {
        self.next_text_item_with_layout(start, property_end, face, DisplayItemLayout::default())
    }

    #[cfg(test)]
    pub(crate) fn source_position(&self) -> DisplaySourcePosition {
        if !self.replacement_strings.is_empty() {
            return self.replacement_strings.source_position();
        }
        DisplaySourcePosition::buffer(self.buffer_id, self.char_pos, self.byte_pos(self.char_pos))
    }

    pub(crate) fn next_cursor_item(
        &mut self,
        context: &mut DisplaySourceContext<'_>,
        replacement_mode: BufferTextDisplayReplacementMode,
    ) -> Option<BufferTextCursorItem> {
        loop {
            if let Some(item) = self.replacement_strings.next_item(context) {
                return Some(BufferTextCursorItem::Item(item));
            }

            if self.char_pos >= self.end {
                return None;
            }

            let start = self.char_pos;
            if self.overlay_strings_produced_at != Some(start)
                && let Some(strings) = self.overlay_strings_at(start)
            {
                // INSERTION: the cursor stays put, so the buffer character at
                // this position is produced next and the strings render ahead of
                // it (GNU handle_stop order).
                self.overlay_strings_produced_at = Some(start);
                return Some(BufferTextCursorItem::OverlayStrings(
                    BufferOverlayStringsItem {
                        anchor_charpos: start,
                        strings,
                    },
                ));
            }
            let property_end = self
                .next_property_change(start)
                .max(start.add_len(CharLen::new(1)))
                .min(self.end);
            let face = self.face_at(start, context);
            let pointer_appearance = self.pointer_appearance_at(start, property_end, face, context);
            let span = self.span(start, property_end);

            if let Some(display_source) = self.display_prop_source_at(start) {
                let display_prop = display_source.value;
                let display_property = DisplayPropertySourcePlan::new(display_prop);
                if replacement_mode.consumes_typed_replacements()
                    && display_property.replacement().is_some()
                {
                    // GNU renders a display REPLACEMENT once over the full extent
                    // of the display value (next_single_char_property_change(pos,
                    // Qdisplay)). The general run breaks at any text-prop/overlay
                    // boundary, so an overlay `display` (e.g. an org inline image)
                    // covering text with internal face/invisible changes would
                    // otherwise replay the replacement once per sub-run.
                    let covered = ReplacementCoveredSpan::for_property_source(
                        display_source,
                        start,
                        property_end,
                        self,
                    );
                    self.char_pos = covered.resume();
                    return Some(BufferTextCursorItem::DisplayPropertyReplacement(
                        self.display_replacement_item(
                            display_prop,
                            display_property.into_classification(),
                            covered,
                        )
                        .with_pointer_appearance(pointer_appearance),
                    ));
                }
                self.char_pos = property_end;
                let item_layout = match self.display_property_cursor_action(
                    context,
                    &display_property,
                    face,
                    span,
                ) {
                    DisplayPropertySourceCursorAction::PushReplacement { value, base_face } => {
                        if replacement_mode.inlines_replacement_strings() {
                            self.push_display_replacement_string(
                                value,
                                base_face,
                                start,
                                property_end,
                            );
                            continue;
                        }
                        return Some(BufferTextCursorItem::DisplayPropertyReplacement(
                            self.display_replacement_item(
                                value,
                                display_property.into_classification(),
                                ReplacementCoveredSpan::for_single_property_run(
                                    start,
                                    property_end,
                                ),
                            )
                            .with_pointer_appearance(pointer_appearance),
                        ));
                    }
                    DisplayPropertySourceCursorAction::Emit(item) => {
                        return Some(BufferTextCursorItem::Item(
                            item.with_pointer_appearance(pointer_appearance),
                        ));
                    }
                    DisplayPropertySourceCursorAction::Skip => {
                        // `(left-fringe …)`: covered text already consumed
                        // (char_pos advanced to property_end); emit no glyph.
                        continue;
                    }
                    DisplayPropertySourceCursorAction::FallThrough { layout } => layout,
                };
                return self
                    .next_text_item_with_layout(start, property_end, face, item_layout)
                    .map(|item| item.with_pointer_appearance(pointer_appearance))
                    .map(BufferTextCursorItem::Item);
            }

            return self
                .next_text_item(start, property_end, face)
                .map(|item| item.with_pointer_appearance(pointer_appearance))
                .map(BufferTextCursorItem::Item);
        }
    }

    pub(crate) fn next_display_item(
        &mut self,
        context: &mut DisplaySourceContext<'_>,
        replacement_mode: BufferTextDisplayReplacementMode,
    ) -> Option<DisplayItem> {
        match self.next_cursor_item(context, replacement_mode)? {
            BufferTextCursorItem::Item(item) => Some(item),
            BufferTextCursorItem::DisplayPropertyReplacement(_) => {
                debug_assert!(false, "display item cursor surfaced a buffer replacement");
                None
            }
            BufferTextCursorItem::OverlayStrings(_) => {
                debug_assert!(false, "display item cursor surfaced overlay strings");
                None
            }
        }
    }
}

impl<B: LayoutBufferView + ?Sized> DisplayItemSource for BufferTextSourceCursor<'_, B> {
    fn next_item(&mut self, context: &mut DisplaySourceContext<'_>) -> Option<DisplayItem> {
        self.next_display_item(context, BufferTextDisplayReplacementMode::InlineSourceItems)
    }
}

/// The producer's cursor is what [`ReplacementCoveredSpan::for_property_source`]
/// asks about the buffer; the rule itself lives with the span, so the covered
/// end cannot be derived anywhere else.
impl<B: LayoutBufferView + ?Sized> DisplayReplacementExtentLookup
    for BufferTextSourceCursor<'_, B>
{
    fn extent_scan_end(&self) -> CharPos0 {
        self.end
    }

    fn extent_overlay_end(&self, overlay: Value) -> Option<CharPos0> {
        self.buffer
            .layout_overlays()
            .overlay_end_emacs_byte_pos(overlay)
            .map(|end| self.buffer.layout_emacs_byte_pos_to_char_pos(end))
    }

    fn extent_display_prop_at(&self, at: CharPos0) -> Option<Value> {
        self.display_prop_at(at)
    }

    fn extent_next_property_change(&self, at: CharPos0) -> CharPos0 {
        self.next_property_change(at)
    }
}
