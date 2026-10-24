# Theorem UI conformance repairs

Started 2026-10-23 against fork revision
`b9d6db9eb26409c2bebab03cb8845be616d4df92` (Servo 0.7, Stylo 0.22).

Source report: Theorem UI `docs/desktop/conformance.md` on
`claude/interesting-bohr-cee321`, inspected at
`0841d567de5731fab94df79ad5b6123aaa2680c0`. Its checkout is
`/Volumes/SSD Samsung/var-18/Theorem ui/theorem-ui-design-pass-ddb5d6`.
The user-facing `/Users/travisgilbert/Theorem ui` checkout is a different
branch and does not contain that report or desktop host.

All twelve report items remain in scope. UI workarounds do not close engine
items. Container-query stacking is the report's accepted UI fallback, but
engine container-query support is still absent. GPUI termination belongs to
the host, outside this engine repair series. The attached architecture
assessment is supporting analysis, not a replacement specification.

## Acceptance

Each repair requires a small reproduction, an engine regression (prefer
existing WPT), and the original UI interaction using that engine. Real input
requirements must cross the embedder input path; dispatched DOM events do not
prove default editing behavior. Actual AppKit IME still needs separate native
acceptance. No consumer revision is promoted from a preference toggle or a
compilation result.

Every row has `oracle_class=live_browser_and_desktop`,
`implementation_mode=production_engine`, `substitution_allowed=false`, and
`live_oracle_required=true`. Evidence and status below are deliberately
separate from that acceptance bar.

## Inventory, using the report's numbering

| Item | Source-grounded starting point | Required evidence | Status / evidence class |
| --- | --- | --- | --- |
| 1 SVG styles | `components/script/dom/svg/svgsvgelement.rs::serialize_and_cache_subtree` clones/serializes SVG into an image URL; page computed styles are not copied by that code | CSS fill/stroke, inherited currentColor, variables and theme mutation in inline SVG; Mermaid/minimap rendered pixels | Open; source inspection |
| 2 :has | Stylo matcher already exists; Servo preferences do not forward its gate. No use of Stylo's relative-selector invalidator found in Servo components | Parsing, descendant attributes, child insertion/removal, preceding siblings, focus and shadow boundaries; Dockview/Milkdown updates | Relative invalidation implemented; 1,444/1,444 assertions across 26 files and live Milkdown updates pass. Dockview focus-visible state and its original rings now pass native UI checks; :has remains disabled by default |
| 3 SVG interaction | SVG `dataset` IDL was commented out; DOMStringMap was restricted to HTMLElement. Focus is also gated on CSS layout boxes in `element/focus.rs` | SameObject and bidirectional dataset reflection; actual path hit-testing after pan/zoom; focus and keyboard deletion | Dataset browser regression 15/15 pass; geometric targeting, focus and UI acceptance remain open |
| 4 caret movement | `document/editing.rs::perform_editing_action` explicitly returns false for MoveCursor | Arrows and Shift-arrows over formatting, wrapping, Unicode and bidi, using actual input; Crepe | Native navigation regressions 9/9 and all 20 line-break navigation assertions pass in final candidate 12; results below. Crepe arrows/Shift and emergency long-word wrapping pass. Advanced caret behavior and bidi remain unqualified |
| 5 word selection | Needs default mouse-selection path investigation | Double-click word selection, punctuation and Unicode outside controls, using actual input | Native inline and noneditable word selection regressions pass, including punctuation and formatting boundaries; selecting a word by double-click passes in Crepe |
| 6 composition | `document_event_handler.rs::handle_ime_event` fires composition events; contenteditable commit remains to implement with event-order/cancellation rules | Composition replacement, cancellation, commit and undo in Crepe, plus native IME | Host-driven composition replacement, commit, clearing preedit and undo/redo pass in Crepe; engine regressions 2/2 pass. Actual AppKit IME remains unverified |
| 7 Range geometry | `range/range.rs::client_rects` explicitly returns whole-node border boxes for partial text; collapsed text also uses whole-node boxes | Partial/collapsed ranges across lines, inline formatting, transforms and scroll; ProseMirror menus | Partial/collapsed text query implemented; 10/10 assertions across six Range files pass, including split RTL ligatures, transforms and scroll. Actual Crepe viewport-coordinate checks pass; exact intra-ligature caret positions remain unqualified |
| 8 HTML drag/drop | DataTransfer support alone does not establish a drag session | Dockview group moves, valid/rejected drop, cancellation and correct drag data/event lifecycle | Open; report evidence |
| 9 container queries | Report identifies parser feature and missing layout size query; `ServoDangerousStyleElement::query_container_size` is todo | Parse + size updates + nested containers, actual canvas split | Open; accepted stacked UI fallback |
| 10 declarations | Must be split into parse/computed/layout/paint obligations | See property inventory below | Open; report evidence |
| 11 canvas fonts | Canvas uses the global FontContext; FontFaceSet adds templates to it. TextMetrics constructor arguments swap font and glyph bounds | Loaded FontFace measurement/pixels; empty-string metrics; PDF embedded fonts without disableFontFace | Metrics browser regressions pass. URL and ArrayBuffer Ahem measurement/pixels already pass on baseline with experimental preferences; PDF-specific font defect remains unresolved |
| 12 PDF text | The full PDF viewer enables text layer; only PdfCard intentionally disables it. pdf.js TextLayer streams extraction into span creation | Separate getTextContent items, stream completion, TextLayer.render settlement and created spans; selection/search in viewer | Open; cause unresolved |

Item 10 property inventory: `text-overflow: ellipsis`; `display: -webkit-box`
and `-webkit-line-clamp`; `vertical-align` on replaced/table content;
`writing-mode`; `backdrop-filter`; `field-sizing`; `appearance`; `resize`;
`content-visibility`; `text-underline-offset`; `text-wrap`; `scrollbar-width`;
`scrollbar-color`; `animation-timeline`. The report's three UI fallbacks also
require tracking `pointer-events: all`, `mask-image`, and
`background-clip: text`. SVG `visibleStroke` belongs to item 3. Vendor-only
control pseudo-elements are not a cross-engine acceptance requirement.

## Initial implementation

Dataset uses the existing DOMStringMap implementation with Element as its
owner. The attribute is moved to the existing HTMLOrSVGElement mixin, with
SameObject and lazily cached SVG maps. No parallel attribute parser is added.
`dataset-svg.html` covers svg, g and path ownership, reflection, enumeration,
invalid names and cloning. Existing HTML dataset WPTs remain unchanged.

`layout_css_has_selector_enabled` forwards the existing Stylo preference and
is tested in both directions. Default remains false. A parser-only change
must not be called completion of item 2 or enabled in the UI before dynamic
invalidation passes.

## Evidence and reproduction

Task receipts, untouched-source backups and build logs:
`/Volumes/SSD Samsung/theorem-servo-ui-conformance-20261023/`.

Initial 12-file WPT run against the existing Theorem binary reproduced :has
failures but also hit the embedder's known single-WebView window-restoration
failure. Re-running files in separate runner invocations avoids conflating
that harness error with a feature failure. Expected-failure metadata is not
rewritten. An improved WPT can report PASS while its runner exits nonzero for
an unexpected pass; inspect actual subtest results.

The initial direct script check omitted js_jit, which forced SpiderMonkey's
source build and exposed its refusal of a canonical registry path containing
spaces. The corrected check enables the product's js_jit feature. The local
Apple linker also emitted malformed proc-macro LINKEDIT pools, confirmed by
a rustc/dyld loader probe. A task-local compiler wrapper uses bundled LLD and
the installed 26.5 SDK for proc macros only; no global compiler settings or
third-party source were changed.

Build and browser result updates follow below.

The TextMetrics call site passed glyph ascent/descent in the font slots and
font ascent/descent in the actual-glyph slots. The patch corrects argument
order without inventing font metrics. The new empty-string/baseline WPT fails
both assertions on the original binary; the existing actualBoundingBox test
also fails there. This is the metrics subproblem of item 11, not proof of a
PDF rendering repair.

The isolated FontFace Ahem probe passes both URL and ArrayBuffer loading on
the original engine with experimental WPT preferences: glyph advance is 50px
and sampled pixels match Ahem. The probe also passes with a detached canvas.
The UI source currently requests Preferences::default(), whose FontFace gate
is false, whereas the report describes an exposed FontFace constructor. That
source/runtime discrepancy must be resolved against the exact measured UI
binary before attributing all PDF-font behavior to a missing engine feature.
The first version of this probe returned a boolean from test cleanup, which
WPT rejects; the corrected run is `baseline-fontface-corrected`.

The standalone shell enables Bluetooth even with default features disabled.
That exposed a pre-existing missing GenericSender import in components/servo;
enabling multiprocess did not fix it. The patch adds the missing import under
the same Bluetooth cfg as its call site. The original Theorem consumer did
not exercise that feature combination. The candidate uses
`js_jit,baked-in-resources,webgl,multiprocess`.
The changed script/config crates pass the focused check, and all three
servo-config unit tests pass. WPT lint accepts the three new browser tests.

The full fork shell then exposed two more omissions in the 0.7 replay:
SimpleDialog's message/confirm/dismiss helpers had been removed while the
shell still called them, and the macOS configuration-directory function had
been replaced by a duplicate Windows function. Restored the helper block and
platform configuration-directory block verbatim from pinned upstream 0.7
`aac43a3f31a259f04a574f5ec4e959c943ad7cc7`. The fork's RustyWeb search-page
setting is preserved. These repairs are required to run the fork's own WPT
browser; they are distinct from the twelve UI feature gaps.

## Verified initial repairs (engine only)

- Built the source-bound patched servoshell successfully. The final executable
  hash and features are in `binary-provenance.json`.
- SVG dataset: new WPT **15/15 PASS**, compared with **0/15** on the original
  engine. Existing dataset.html improves from 6/8 to 7/8; the remaining
  MathML case is outside SVG support. Existing HTML dataset tests still have
  their pre-existing toString, invalid-name and deletion failures. Binding
  prototype-setter cases remain 2/4 PASS on both engines; prototype tests
  remain 2/2 PASS. The 12-file patched run finishes all harnesses OK, with
  61 passing and six failing subtests; the remaining six failures also occur
  on the original engine.
- Canvas: new empty-string/baseline metrics WPT **2/2 PASS**, compared with
  **0/2** on the original engine. Existing actualBoundingBox WPT improves
  **0/1 to 1/1**. Existing Ahem font-bounds WPT **1/1 PASS**. Loaded FontFace
  measurement/pixel probe **2/2 PASS** on both original and patched engines
  under experimental WPT preferences.
- No WPT expectations were relaxed. Unexpected passes in retained upstream
  failure metadata can make the runner exit 1 even when that test now passes.
- These are actual browser tests, not the original UI acceptance: the UI still
  points at the published b9d6 revision. No desktop install or production pin
  has been changed. SVG shape hit testing and focus, PDF rendering and text
  layer behavior, and the other table rows remain open.

## :has() qualification result

With `--pref layout_css_has_selector_enabled=true`, on the patched browser:

| Existing WPT | Actual results |
| --- | --- |
| has-basic.html | 18/18 PASS |
| has-relative-argument.html | 35/35 PASS |
| has-focus-display-change.html | 0/1 PASS |
| invalidation/has-in-parent-position.html | 160/211 PASS |
| invalidation/has-sibling-insertion-removal.html | 14/28 PASS |

All five harnesses completed with status OK; the subtest failures are real
feature failures, not the earlier embedder window-restoration error. The
preference stays disabled by default and is not added to experimental
feature bundles. A host-only toggle would restore parsing while leaving
focus/ancestor/sibling styles stale. The next engine step is to integrate
Stylo's existing relative-selector invalidation for state/attribute snapshots
and structural mutations, then rerun these same unmodified WPTs and the UI
focus/table/image-handle cases. A whole-document restyle workaround is not
introduced.

No branch push, upstream submission, consumer pin change or installed-app
replacement was performed. The source remains on the existing fork branch;
the pre-existing REPLAY-CHECKPOINT.md edits were preserved untouched.

## Continuation: relative invalidation and editing

The user's next requested order is :has invalidation, then editing, with actual
Theorem UI validation. Work is in progress; the preference remains false.

The first relative-invalidation integration connects state/attribute snapshots
(including unstyled descendants) and insertion/removal to Stylo's existing
invalidator. Mutation invalidation runs after insertion and before removal so
ancestry is still available. Dirty ancestor paths expand the traversal root;
this does not add a whole-document restyle hint. Appends use Stylo's Append
operation, and changes to adjacency use its sibling traversal overrides.

`has-invalidation-first/` records 293/293 assertions passing across all five
original tests, including focus, 211 parent-invalidation assertions, and 28
sibling insertion/removal assertions. A broader batch (`has-expanded-2/`)
records 1430 passing and eight failing assertions, plus the large complexity
test timing out. The first batch launcher failed due to a missing Python
multiprocessing main guard; `has-expanded/` is a harness failure, not engine
evidence.

The eight failures reduce to an early return in Stylo 0.22's
AncestorEarlierSibling walk. An irrelevant sibling of an intermediate ancestor
must stop that sibling loop, not traversal of higher ancestors. The pinned
published Stylo source is in `support/downstream/vendor/stylo`, with the one-line
correction and provenance in THEOREM-PATCH.md. The new
has-inserted-subtree-non-subject.html regression fails before this correction
(`has-subtree-debug/`). Revalidation of the corrected dependency is pending.

An isolated copy of Theorem UI's desktop host is built in the evidence folder
as `ui-desktop-candidate/`, using a Cargo path override for this fork. Its only
host behavior difference is a dedicated profile path; its server still runs
the actual UI production build and live local backend at 8084. Neither the
shared UI checkout nor its published Servo revision was changed. `ui-initial/`
and `ui-note-baseline/` contain host-driver reports and Servo screenshots: the
real application loads, opens an explicitly created test Markdown file, and
mounts its Crepe/ProseMirror editor and table. This establishes the acceptance
path, not evidence that the editing defects are repaired.

### Editing implementation and first runtime checks

The first candidate adds DOM text-position mapping backed by Servo's existing
Rope, native double-click word selection, contenteditable composition replacement
through the existing insert-text command, and a batched shaped-text Range query.
No new Unicode segmentation library or parallel editing command engine is used.
These changes are still under qualification, not all-gap completion.

`editing-first` and `navigation-actions` preserve the initial results. The
original send-keys test hit a keyboard-types 0.8.3 WebDriver modifier-release
panic; explicit WebDriver Actions avoid that runner defect while still sending
trusted browser input. Navigation passes inline-formatting arrows/Shift,
grapheme sequences, double-click, and canceled keydown (4/5); the rendered-line
column check fails by one character. Range partial text/caret/wrapping,
surrogate indexing, and all four shadow/disconnected tests pass. RTL split
ligature, nested-text rectangle count, and display:contents tests expose further
edge cases. Those remain open until rerun after corrections.

The mounted Theorem UI run `ui-editing-first-after` verifies actual Crepe arrows,
Shift-selection, selecting `bold` by double-click, and composing `に` then `日本`:
the note ends with one `日本`, and logged trusted composition/beforeinput/input
ordering shows replacement rather than duplicated preedit. Milkdown's original
`:has(.ProseMirror-selectednode)` table-cell CSS switches none -> solid 1px ->
none when the child selection class changes. The candidate alone enables the
preference for this test. The Range query still reports reference-frame-relative
positions in this UI; that runtime finding is being corrected before acceptance.
Native AppKit IME and commit/undo/cancellation acceptance are not yet established.

A test-only UI server survived graceful shutdown and held the isolated fixture
workspace database. Its PID and log ownership were checked before termination;
no user workspace database was removed. Subsequent UI runs must either confirm
that fixture server has exited or use a fresh isolated profile. Earlier failed
setup reports (`ui-editing-before`, `ui-editing-before-opened`) are not editing
evidence. `ui-editing-before-recovered` is the valid negative control.

### Additional qualification findings

The Stylo sibling correction now passes both 295-assertion adjacent/sibling
suites and its new minimal regression (`has-stylo-fixed`). Text changes affecting
`:empty` also have a failing-before/passing-after regression (`has-empty-before`,
`editing-first`). A deeper class-change-then-detach regression still failed
(`has-detach-deep-before`): queued snapshots need invalidation while the old
ancestry exists. The candidate processes pending snapshots in the removed
subtree before unlinking it, using cloned Stylo snapshots; validation is pending.

The large `has-complexity` test also times out on the preserved baseline.
The isolated scaling probe reports baseline runtimes of 976.7 ms at 1,000 nodes,
1,686.06 ms at 2,000, and 10,498.76 ms at 4,000 (timeout). The candidate's earlier
2,000-node run took 3,934.51 ms while other builds were active. These are diagnostic
runs, not a controlled performance comparison or evidence of acceptable scaling.

`range-debug-3` verifies the corrected reference-frame transform and
`display:contents` handling. Remaining nested-whitespace and split RTL ligature
failures led to using the existing offset map across the entire inline formatting
context and preserving the actual character origin of shaped slices. This avoids
assigning the same shared ligature to conflicting DOM offsets. Those corrections
are awaiting the next stable-source browser build.

Composition input events now populate `beforeinput` target ranges and leave
`input` ranges empty. Synthetic InputEvent construction preserves supplied ranges
and the composed flag; listener-side range access reuses DOM retargeting to hide
shadow-tree endpoints from outside listeners. Native composition and existing
shadow-range WPTs will validate the behavior.

### Functional invalidation qualification

`has-functional-qualified/report.json` records **1,444/1,444 PASS across 26
harnesses, all OK**. This includes focus, ancestor and sibling invalidation,
attribute/state snapshots, inserts/removals, nested non-subject selectors,
`:empty` text changes, moving a subtree, mutation-before-detach, and shadow-tree
boundaries. The existing WPT failure metadata is retained; its unexpected passes
explain the nonzero runner status. No broad subtree restyle workaround was added.

`editing-qualified-6` passes the split RTL ligature, display:contents, partial
text/transforms, surrogate indexing, all four shadow/disconnected range cases,
composition commit/cancel (2/2), and InputEvent constructors (5/5). The synthetic
shadow-range test passes. Its two native shadow typing cases fail at WebDriver's
send_keys interactability gate before typing, so they do not qualify that path.

The geometry diagnostic (`geometry-probe.json`) explains the remaining nested
range rectangle count: `Firefox again` becomes three word/space fragments with
identical font and baseline. The next query joins contiguous fragments in the
same font/reference frame before transforming their rectangles. The diagnostic
also measures `fi` as one monospace cell in this environment (12.0333px, versus
24.0667px for `se`); the original vertical-column fixture therefore did not have
equal character columns. Its new deterministic Ahem fixture avoids that font
fallback behavior, and a separate Ahem test covers movement across a soft wrap.
The unchanged upstream line-break/click suite is 4/20; failures include initial
pointer placement before navigation and remain an explicit qualification gap.

The remaining long-word caret fixtures expose a separate layout defect, not a
second rendered line: `wrap-probe.json` shows `overflow-wrap:anywhere` computes
successfully, but a 100px Ahem block still produces one 200px text rectangle and
one 30px line. The unchanged upstream break-word/click suite is 0/20 on the old
engine and 4/20 on the candidate (explicit `<br>` cases pass). Keep its 16 failures
open as layout/long-word wrapping debt; do not claim those navigation cases pass.
The focused soft-wrap navigation test uses a normal word boundary and first
asserts that two lines actually rendered.

Text hit-testing also had a coordinate-space inconsistency: vertical-bound
checks added the parent-relative text-fragment origin to an already-local point.
The candidate now compares against zero and the fragment height, consistent with
its distance calculation. This is separate from absent emergency word wrapping.

The final multi-file run exposed a WebDriver race: Element Send Keys queues IME
messages with acknowledgement receivers but only the keyboard-action path drains
those receivers. It can return before composition edits reach the document.
The candidate now reuses the existing input-acknowledgement wait after each
composition event. The unchanged composition regression remains an immediate
post-send assertion, so this repair is tested rather than hidden behind sleeps.


## Earlier qualification checkpoint, 2026-10-23 (superseded by candidate 12)

The final stable-source shell (`servoshell-editing-9`) reports **1,493 PASS,
18 FAIL, and all 40 harnesses OK** in `final-engine-qualified-9/report.json`.
All 1,444 relative-selector assertions, all eight native navigation assertions,
all ten Range assertions across six files, both composition assertions and all
five InputEvent-constructor assertions pass. The composition test still checks
immediately after WebDriver Send Keys; the acknowledgement repair removes the
race without adding a sleep. WPT expected-failure metadata remains unchanged.
The final WPT lint command succeeded (`final-wpt-lint.log`, exit 0), and
`git diff --check` succeeded.

The 18 remaining assertions are explicitly not passes: 16 in the unchanged
long-word line-break suite and two shadow-root typing cases rejected at the
WebDriver interactability gate. The synthetic shadow InputEvent retargeting
case passes. Existing negative controls and earlier failed receipts remain
available; they are not overwritten by the final reports.

`ui-final-clean-9/report.json` contains 66 completed driver steps with no driver
errors. It runs the actual production Theorem UI and Crepe against the candidate
engine through the GPUI native input path, using only task-owned Markdown notes
in an isolated profile. It verifies ArrowRight, Shift-selection, double-click
word selection, Japanese preedit replacement and commit, trusted input-event
ordering/target ranges, undo/redo, subsequent typing/backspace, empty commit
removing preedit, and partial/collapsed Range rectangles in viewport coordinates.
The screenshot `ui-final-clean-9/verified.png` was visually inspected. Milkdown's
original table-selection CSS updates none -> solid 1px -> none when its child
selection class changes. No product CSS workaround was added.

The final two Dockview diagnostic steps are observations, not passing focus-ring
assertions. The real close action has a visible layout box and receives focus,
but `:focus-visible` remains false even following keyboard input. The source
matcher reads `ElementState::FOCUSRING`, while no Servo component sets that bit.
Consequently the original `.dv-tab:has(:focus-visible)::after` rule cannot match.
This is a separate missing focus-visible state policy, not evidence that the
relative-selector invalidator drops a state transition. The earlier
`ui-final-9` report contains that failed assertion plus a test-only wait for a
selectionchange after collapsing to the already-current caret; the final run
checks the actual caret position instead. Neither failed receipt is erased.

### Remaining acceptance and next work

- Implement focus-visible input-modality/state policy using the existing focus
  machinery, then qualify keyboard/pointer/script focus and Dockview's original
  ring. Keep the has preference opt-in until performance and product acceptance
  are satisfied; the large-tree complexity test has not been qualified.
- Fix missing emergency long-word wrapping, then rerun the 16 line-break cases.
  The implemented navigation uses logical grapheme/word order; visual bidi,
  sticky horizontal position across short lines, atomic/readonly/empty-block
  caret stops, and true page navigation are not qualified. Page currently uses
  the adjacent-line path. These limitations do not close the entire caret gap.
- Run an actual OS/AppKit input-method session for commit/cancel/undo. The driver
  verifies host-to-engine composition, not an OS input method. Resolve shadow
  WebDriver interactability to qualify native shadow-root editing. Intra-ligature
  collapsed caret placement and ProseMirror popup interactions need further
  acceptance beyond the verified Range rectangles.
- The other inventory items remain open as listed above. In particular SVG
  paint/targeting, HTML drag/drop, container queries, unsupported declarations,
  embedded PDF fonts and the PDF text layer are not repaired by this checkpoint.

### Source and binary preservation

`editing-final-provenance.json` records source, binary, test, receipt and consumer
hashes. `servo-ui-editing-final.patch` contains tracked changes excluding the
pre-existing `REPLAY-CHECKPOINT.md` edit. `servo-ui-editing-final-source.tar.gz`
contains the changed and new source files, including the exact Stylo vendor
crate and regression tests. Both are recoverable supplements to the retained
fork checkout, whose branch and remote still start at
`b9d6db9eb26409c2bebab03cb8845be616d4df92`; no commit or push was made here.

The isolated consumer is `ui-desktop-candidate`, built with a local fork path
patch and the has preference enabled only for qualification. The published UI
Servo pin and installed application were not promoted. During this session the
shared UI checkout advanced from `0841d567` to `09f757bc` for its folder-index
work; there are no desktop-source differences between those revisions. Its
existing untracked `ide/` directory was left alone. The final evidence captures
hashes of the production assets used by the test instead of claiming that an
untracked build output is described by Git HEAD alone.

Task-owned fixture servers were checked by database and log file descriptors
before termination where needed. At the final cleanup check no process held the
fixture database. The shared backend at port 8084 was not stopped. Candidate
source, isolated profile, fixtures, binaries and receipts are retained for
reproduction; no new Git worktree was created in this session.

## Follow-up: focus-visible, wrapping and editing

The next candidate (`servoshell-focus-wrap-10`) sets Stylo's existing FOCUSRING
state through DocumentFocusHandler. Keyboard modality is recorded before event
dispatch, including canceled keydown; shortcut modifiers do not change it.
Pointer input changes the policy for subsequent focus changes. Text controls
and editable hosts indicate focus, blur clears the state, and input-type changes
refresh it. Shadow hosts retain their existing focus retargeting without copying
the descendant's focus-visible state. The policy follows the suggestions in
<https://drafts.csswg.org/selectors/#the-focus-visible-pseudo> and is being checked
against the existing focus-visible WPT suite.

Emergency wrapping reuses ICU's grapheme segmentation and Servo's shaped-text
slicer. The layout-specific addition records fallback breaks within an otherwise
unbreakable segment, uses them only after ordinary word boundaries are exhausted,
and coalesces text again before creating rendered fragments. Grapheme boundaries
are computed over the whole inline formatting context, including font and inline
element boundaries. The fallback consumes candidates and line items once rather
than repeatedly copying the remainder of a long word. Intrinsic sizing includes
these breaks for anywhere and excludes them for break-word, as specified by
<https://drafts.csswg.org/css-text/#overflow-wrap-property>. This custom glue is
needed because the existing layout only committed whole unbreakable segments;
Unicode segmentation and shaping algorithms are reused rather than replaced.

Selection now remembers a visual line coordinate and preferred horizontal
position for vertical navigation. A mouse click supplies the initial line
coordinate, resolving ambiguous DOM offsets at a soft wrap; normal selection
changes clear the remembered position. The WebDriver interactability check also
accepts an already-focused shadow host, preserving its focused descendant as the
keyboard target rather than rejecting Send Keys before editing begins.

`focus-wrap-before-10` preserves the prior candidate's failures: the original
seven-assertion wrapping fixture is 4 PASS / 3 FAIL, and all four focus-visible
behavior assertions fail (the parser assertion passes). The expanded fixture
adds intrinsic sizing and grapheme checks, and the navigation fixture adds a
short-line column-retention test. WPT lint passes for both changed fixtures.
The first focused checks found and led to repairs for an unnecessary no_trace
annotation on a scalar Cell and cloning the shaped-slice type. The complete
shell build then succeeded. Candidate 10 is superseded by the final follow-up below. The earlier
qualified checkpoint and binaries remain preserved.


### Focus, wrapping, and editing follow-up: candidate 12

The focus policy now drives Stylo's existing FOCUSRING state. Keyboard input
updates it before key handlers run, access-key focusing restores keyboard
indication, text-editing controls retain indication after pointer focus, and
clicks on non-focusable content preserve the policy for subsequent script focus.
The UA styles use `:focus-visible` with `outline: auto`; the old control-specific
`:focus` suppression is removed. Author styles remain responsible for Dockview's
own ring. The focus suite covers modifier keys, canceled key events, input type
changes, shadow hosts, access keys, and pointer/script transitions.

Normal text insertion now provides `data`, `inputType`, and a snapshot of the
insertion range on `beforeinput`; input events cross shadow boundaries while the
existing target-range filtering protects shadow internals. This follows the
Input Events Level 2 insertion requirements:
<https://www.w3.org/TR/2026/WD-input-events-2-20261015/#interface-InputEvent>.
Deletion-range calculation is not implemented by this insertion-specific change.
Focusing another editing host replaces the previous selection. Focusing the same
host preserves any selection already inside it, including a new Range installed
by Selection.collapse(), rather than requiring identity with a cached Range.

Candidate 11's broader selection test caught the return-focus regression in both
inline and block editing hosts (389 PASS / 3 FAIL across 63 harness results).
Candidate 12 repairs that guard. The other failure is document autofocus: the
button is never focused, so the focus-visible assertion cannot be reached. The
negative control `editing-host-before-11` records the previous qualified binary
keeping selection in the first editor after focus moves to the second. The new
regression also checks insertion event data/ranges and canceled insertion.

The final native UI run is `ui-focus-wrap-final-12/report.json`: 85 steps, zero
driver errors, using `theorem-focus-wrap-12` and the existing production UI assets.
The asset receipt hashes 1,043 files; their latest modification is
2026-10-23T23:04:15Z, before the native UI runs. This is a newer built bundle
than the candidate 9 checkpoint, and is recorded independently of the concurrently
changing UI checkout HEAD. The candidate desktop host source itself is unchanged.
It verifies the original Dockview descendant ring, native Shift-Tab/Tab navigation
back to the tab itself, and clearing both rings on blur. In the visible Crepe
editor it checks caret/selection movement, double-click word selection, composition
replacement and clearing, undo/redo, viewport Range geometry, and the original
Milkdown relational-selector rules. A second real Markdown note contains a long
word: its text ranges wrap within the paragraph width, and clicking then moving
the caret keeps the editor interactive. These are native GPUI-to-Servo input
operations; physical AppKit input-method operation remains unverified.

Servo's screenshot callback stalled after opening the second note in both the
probe and final capture runs. Their partial reports remain preserved (77/80 and
84/87 steps, respectively). The first three captures, including both focus rings,
succeeded. A macOS capture of the exact candidate window confirms the wrapped
word visibly occupies four lines: `ui-focus-wrap-12/crepe-wrapping-os.png`.
The final 85-step completion run omits the two stalled capture requests and keeps
all interaction and geometry assertions, then completes the normal quit step.
`ui-focus-wrap-acceptance-12.json` binds these receipts without treating the
stalled callback as passing. The callback failure remains open.

Oracle class: live browser engine and native product UI. Implementation mode:
real. Evidence class: WPT execution, native UI input and DOM assertions, and
rendered native-window captures. Substitution allowed: false. Live oracle
required: true.

Final combined engine-suite receipt: `final-focus-wrap-engine-12/report.json`
contains **1,871 PASS, 1 FAIL, and all 100 harness results OK**. The only failure
is `/css/selectors/focus-visible-009.html`: document autofocus leaves the body
focused instead of the button. All 11 emergency-wrapping geometry assertions,
all nine native-navigation regressions, all 20 upstream line-break navigation
assertions, and all 151 initial-selection-on-focus assertions pass. The combined
run also rechecks relative-selector invalidation, focus policy, Range geometry,
shadow-root typing, insertion events, composition, SVG dataset, and canvas/font
metrics. Existing WPT expected-failure metadata is retained, so unexpected passes
cause the runner's nonzero exit status. WPT lint for all 12 added or modified
HTML/JS test files passes; both candidate binaries build successfully.

`focus-wrap-final-provenance.json` binds the final binaries, source snapshot,
production asset hashes, test reports, and native UI receipts. The recoverable
source archive and tracked patch exclude the concurrent checkpoint edit. The
verified patch set is prepared for commit and push on
`Travis-Gilbert/servo07-browser-20261013`; the provenance records the resulting
commit and verified remote head after publication.

Qualification remains bounded: document autofocus, physical OS IME integration,
visual bidi/vertical/page caret behavior, scrolling between cached caret positions,
and emergency wrapping with mixed line heights, floats, or cloned inline box
borders require additional coverage. The earlier twelve-gap inventory is not
closed by this follow-up. The production Servo pin and installed app are unchanged;
this is a qualified fork candidate. The existing source worktree is retained for
the ongoing patch queue and concurrent work. REPLAY-CHECKPOINT.md is excluded
from this task's commit and source archives.


## Compatibility follow-up, 2026-10-24: document autofocus

The user prioritized document autofocus after the broader compatibility audit.
The baseline `autofocus-before/report.json` under
`/Volumes/SSD Samsung/theorem-servo-compat-20261024` records 27 failed and 12
passing assertions across 28 harness results on the previously qualified binary.
The exposed attribute had no document queue or rendering-lifecycle processing.
The implementation follows the HTML autofocus insertion and flushing algorithms,
reuses the existing focusable-area/delegation and focus machinery, and processes
candidates before scroll and animation callbacks. No dependency duplicates those
engine-owned document lifecycle operations.

The audit also found that DocumentOrShadowRoot.activeElement consulted the
Window's active Document instead of the queried node's own Document, giving null
for detached/cloned document bodies. The accompanying ownership fix is checked
against the existing active-element and inactive-document tests.

Related regressions required fixes in the shared focus lifecycle: image-map
links must remain focusable during focus fixup, using the visibility of their
mapped image; a focusable shadow host queues before its descendants; local
iframe ancestors expose the focus chain before animation callbacks; initial
fragment targets are established before load listeners inspect them. The area
coords and shape IDL reflectors now use the existing attribute helpers. Dialog
opening consumes pending document autofocus, and closing a nonmodal dialog now
checks the correct ancestry direction before restoring focus without scrolling.

The 11 lifecycle assertions and two direct keyboard-input variants fail on the
previous binary. Keyboard tests send WebDriver key actions to current focus,
without clicking or calling focus() or send_keys(element), so they exercise
immediate typing in both an input and a contenteditable editor.

The native acceptance fixture is prepared against the previously qualified UI
revision 16b5f3892dadab123270fbddfb473f51b495cbcb in the isolated
ui-source-16b5f389 directory. Its frozen dependency installation and production
bundle build pass. The shared UI checkout is in a concurrent merge and is not
modified. The 74 native host source files match the prior qualified host; only
the path-patched Servo library changes. The running local backend is preserved.
macOS reports the screen locked; the attempted native baseline logged a display
link creation failure and could not provide a window screenshot. Its task-owned
processes were stopped. A locked-screen run is not native UI acceptance.

Candidate 5 builds both servoshell and the native Theorem host with Rust 1.97.1.
The broad regression receipt `regression-after-5/report.json` contains 144 OK
harnesses, 1,999 PASS assertions and two pre-existing FAIL assertions. The
comparison against preserved baseline reports records 1,947 PASS-to-PASS,
47 FAIL-to-PASS, three TIMEOUT-to-PASS, two FAIL-to-FAIL and no PASS regressions;
two further existing shadow-root assertions pass without a recorded baseline.
Both keyboard-input variants pass. WPT lint for both new test files and git diff
--check pass. The 24 affected expectation files are refreshed for the observed
fixes; remaining failure expectations are preserved. The final focused rerun
(`focus-final/report.json`) exits zero: all 45 harnesses run as expected.

Two distinct compatibility gaps remain visible in this suite. The upstream
update-the-rendering test requests a 100px popup but Servo gives it a 1024x740
viewport, so its 200px offset never needs scrolling. `popup-size-probe.json`
records that geometry; the new test using the actual viewport proves autofocus,
scroll, then animation-frame ordering. A control slotted into a nonmodal dialog
still fails focus restoration. The ancestry correction fixes ordinary and
shadow-descendant restoration while preserving focus deliberately moved outside
the dialog. Cross-origin delegated autofocus remains limited by Servo's missing
Permissions Policy implementation; this patch enforces the default same-origin
ancestor boundary and the existing sandbox automatic-features restriction.

Evidence and commands are under
`/Volumes/SSD Samsung/theorem-servo-compat-20261024`; `autofocus-provenance.json`
binds the source hashes, binaries, baseline and final receipts, UI asset digest,
and publication commit. `servoshell-autofocus-5` and `theorem-autofocus-5` are the
preserved candidates. The recoverable changed-source archive and patch exclude
the concurrent REPLAY-CHECKPOINT.md edit and all private runtime environment
files. The fork source checkout is retained for ongoing and concurrent work.
The production Servo pin and installed app have not been promoted.

Engine oracle class: live browser engine. Implementation mode: real. Evidence
class: rebuilt-engine WPT and direct WebDriver keyboard input. Substitution
allowed: false. Live oracle required: true. Engine behavior is verified within
the coverage above. Native UI oracle class: live product UI. Implementation mode:
real. Evidence class: build passed, visible acceptance blocked by the locked Mac.
Substitution allowed: false. Live oracle required: true. Native UI qualification
remains open; the prepared next command runs `theorem-autofocus-5` with
THEOREM_APP_DIR pointing at ui-source-16b5f389 and THEOREM_DRIVE at
ui-autofocus.steps.json after unlocking the Mac. The prepared standalone
native-fixture.steps.json checks typing after autofocus separately from product
UI assertions, and must not be presented as product UI acceptance.
