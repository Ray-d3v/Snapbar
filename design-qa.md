# Native title-bar idle state — Product Design QA

## Evidence

- Design reference: the user-provided Dynamic Island sketch at `C:\Users\arare\.codex\codex-remote-attachments\01a0417a-1d1b-7c10-a5e1-ab0cec13a9a5\316DDA94-0204-472A-BDDF-F313571AB8C4\1-写真1.jpg`.
- Before, unhovered: `target/product-design-audit/native-idle/02-current-titlebar-full.png`.
- Final, unhovered: `target/product-design-audit/native-idle/07-final-unhovered-full.png` and `08-final-unhovered-center.png`.
- Final, hovered: `target/product-design-audit/native-idle/09-final-hovered.png`.
- Required combined comparison input: `target/product-design-audit/native-idle/10-before-after-comparison.png`.
- Runtime context: active one-person Teams meeting, dark title bar, no shared content, 1920-pixel-wide window, Windows DPI 96 (100%). Full title-bar captures are 1920 × 120 native pixels; centered state captures are 720 × 120 native pixels.

## Audit and iterations

### Pass 1 — current implementation

- P1, shape and surface: the 92 × 38 opaque-black container, shadow, rounded drop, and `Snapbar` label read as an attached third-party badge rather than part of the title bar.
- P2, spacing and layout: the idle surface extended 8 pixels below the 30-pixel caption band and was denser than the native caption controls shown in the same screenshot.
- P2, state contrast: idle and expanded modes used the same opaque-island material, so hover disclosure did not create a meaningful native-to-island transition.

### Pass 2 — first native-cell build

- Converted idle mode to two 46 × 30 title-bar cells and reserved the 272 × 38 black surface for expansion.
- P2, state rendering: `04-final-unhovered-full.png` still showed a stale camera-cell hover backplate while the pointer was outside the overlay. Removed the GPUI idle hover backplate instead of accepting an incorrect rest state.

### Pass 3 — final build

- `07-final-unhovered-full.png` shows no idle fill, shadow, brand label, rounded container, or caption-bottom drop. Only the centered status dot and 16-pixel camera glyph remain.
- The two cells occupy the title-bar band from its top to bottom and align to the same visual axis as the Windows caption glyphs at the right edge.
- `09-final-hovered.png` confirms that the black square-top, rounded-bottom island appears only after disclosure and that its existing controls remain intact.
- No actionable P0, P1, or P2 visual mismatch remains in the compared states.

## Mandatory fidelity surfaces

- Fonts and typography: idle mode contains no text, eliminating the previous competing micro-label. Expanded Japanese status text remains centered, legible, and untruncated.
- Spacing and layout: idle is exactly 92 × 30 logical pixels, split into two 46-pixel cells. Expanded remains 272 × 38 with its existing 6-pixel control rhythm and 8-pixel drop. Unit coverage verifies 100% and 150% geometry plus narrow-window compact placement.
- Viewport resilience: normal width uses the centered two-cell idle affordance; constrained title bars reduce to one 46 × 30 camera cell; insufficient safe spans still hide the overlay rather than overlapping caption controls.
- Colors and tokens: idle material is transparent with a one-alpha hit surface; green/amber/red status and white/gray camera glyphs remain the only visible tokens. Opaque `#050506` is reserved for expanded mode.
- Image and asset fidelity: the existing repository camera, folder, refresh, and power SVG assets are retained. No substitute illustration, CSS art, or generated asset was introduced.
- Copy and content: `Snapbar` is removed from idle mode. Expanded mode preserves the useful localized status label and current actions.
- Icons: the idle camera is 16 pixels and optically centered in a 46 × 30 cell; status dot is centered in the adjacent cell. Expanded icons remain aligned and visually unchanged.
- States and interactions: pointer enter and leave retain the native 50 ms disclosure controller. Live captures verify both final states; region tests verify that idle/compact regions are rectangular while expanded keeps the rounded-bottom silhouette.
- Accessibility: the 46 × 30 targets match the practical size of Windows caption cells and expanded mode supplies text in addition to status color. The deliberate `WS_EX_NOACTIVATE` contract means this overlay remains pointer-operated and does not take keyboard focus from Teams; UI Automation naming and high-contrast appearance remain separate product follow-ups rather than visual-fidelity claims.

## Result

Final result: passed. The unhovered affordance now reads as title-bar glyphs; the Dynamic Island material is reserved for hover disclosure.
