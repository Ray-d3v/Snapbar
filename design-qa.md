# Dynamic Island disclosure — Product Design QA

## Evidence

- Design reference: the user-annotated root-curve sketch at `C:\Users\arare\.codex\codex-remote-attachments\01a0417a-1d1b-7c10-a5e1-ab0cec13a9a5\5636C9E4-3757-4774-A08F-BCA346C83689\1-写真1.jpg`.
- Initial motion: `target/product-design-audit/island-motion/pass1/expand-contact-sheet.png`.
- Final expansion and collapse: `target/product-design-audit/island-motion/pass5/expand-contact-sheet.png` and `collapse-contact-sheet.png`.
- Final settled state: `target/product-design-audit/island-motion/pass5/expand-07-300ms.png`.
- Required combined reference/implementation input: `target/product-design-audit/island-motion/pass5/reference-implementation-comparison.png`.
- Final primary-action anchor: `target/product-design-audit/camera-anchor/pass1/expand-00-000ms.png` and `expand-07-300ms.png`.
- Runtime context: active one-person Teams meeting with no shared content, dark title bar, 1920-pixel-wide window, Windows DPI 96 (100%). The audit build temporarily allowed local screen capture; release capture exclusion is restored after QA.

## Audit and iterations

### Pass 1 — shallow attached strip

- P1, shape and surface: the 8-pixel drop placed most of the shoulder inside the black caption band, so the root read as a short diagonal edge instead of the annotated concave join.
- P2, motion: the surface widened from the center, but clamping the spring at its target removed the requested Dynamic Island-like peak bulge.

### Passes 2–4 — visible root and spring tuning

- Increased the fixed envelope to 280 × 48, the expanded surface to 272 × 46, and the caption drop to 16 pixels so the join is visible below Teams' title bar.
- Made one progress value drive both GPUI painting and the Win32 input region, eliminating geometry/input drift during disclosure.
- Added a bounded spring overshoot. P2 remained when the first spring peaked too late; stiffness and damping were retuned so useful controls appear around 150–200 ms and the island settles by about 300 ms.
- P2, shape tangent: the first smoothstep shoulder still left the caption edge at a slightly diagonal-looking tangent.

### Pass 5 — final build

- The shoulder now follows a quarter-circle ease-out: it leaves the title-bar baseline horizontally, turns inward across 10 pixels, and reaches the body with a vertical tangent.
- The settled island uses a 16-pixel shoulder inset, 6-pixel bottom radius, and 16-pixel caption drop. The black surface remains one centered horizontal control row.
- Expansion starts after the existing 50 ms pointer verification, grows symmetrically, shows a restrained peak bulge near 200 ms, and returns to the 272-pixel target by 300 ms. Collapse preserves the same silhouette while fading back to the transparent 92 × 30 idle cells.
- A final geometry clamp keeps that peak bulge horizontal; it cannot exceed the audited 16-pixel vertical drop. The settled comparison state is unchanged.
- The combined comparison confirms the two annotated root curves, centered growth, title-bar attachment, and the absence of an unrelated downward menu. No actionable P0, P1, or P2 visual mismatch remains.

### Pass 6 — stable primary-action anchor

- P1, interaction stability: pixel measurement of the previous build placed the idle camera at X 382.5 and the expanded capture button at X 349.5, forcing a 33-pixel pointer correction as disclosure completed.
- Reordered the context row to status, save destination, capture, rescan, and quit, then applied a shared logical anchor to the idle glyph and red capture button. Secondary controls now disclose around the primary action.
- Live Teams captures measure both the idle camera glyph and expanded red capture button at X 382.5, for a 0-pixel horizontal delta. The capture button also stays on that anchor through the content crossfade.
- Unit coverage locks the two logical centers together and verifies that the reordered row remains within the island bounds. No actionable P0, P1, or P2 mismatch remains.

## Mandatory fidelity surfaces

- Fonts and typography: the expanded Japanese status remains legible and untruncated throughout the crossfade; idle contains no competing label.
- Spacing and layout: idle remains 92 × 30; expanded settles at 272 × 46 inside a fixed 280 × 48 envelope. The capture target keeps one X anchor while secondary controls balance around it on one row, clear of Teams caption buttons.
- Viewport resilience: normal width uses the full island, constrained title bars retain the existing 46 × 30 camera-only mode, and insufficient safe spans still hide the overlay.
- Colors and tokens: transparent native-style idle cells and existing status/icon colors are unchanged. Opaque `#050506` remains expanded-only.
- Image and asset fidelity: existing camera, folder, refresh, and power SVG assets are retained; no replacement or generated artwork was introduced.
- Copy and content: status and action semantics are unchanged.
- Icons: all five expanded items remain optically aligned and were checked at settled, expanding, and collapsing frames without clipping.
- States and interactions: enter, peak, settled, leave, and idle frames were captured. Unit coverage checks 100% and 150% geometry, root scanlines, fixed-envelope overshoot, native-region/client-origin alignment, compact placement, and the existing 50 ms disclosure state machine.
- Accessibility: GPUI's reduced-motion setting snaps the spring to its target. The overlay deliberately remains non-activating and pointer-operated so Teams retains focus; keyboard operation and UI Automation naming remain separate product follow-ups rather than claims of this visual change.

## Result

Final result: passed. The surface grows with a visible concave root and restrained spring bulge while the primary capture target remains horizontally stationary from idle through expansion.
