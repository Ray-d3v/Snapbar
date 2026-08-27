# Dynamic Island title-bar design QA

## Evidence

- Source visual truth: `C:\Users\arare\.codex\codex-remote-attachments\01a0417a-1d1b-7c10-a5e1-ab0cec13a9a5\CFDA1861-ABF8-4AF4-9759-38885C9A3D7F\1-写真1.jpg`
- Collapsed implementation: `target/design-qa/collapsed.png`
- Expanded implementation: `target/design-qa/expanded.png`
- Combined comparison: `target/design-qa/comparison.png`
- State: live one-person Teams meeting, dark appearance, no shared content.
- Viewport: a 720 × 120 native-pixel crop around the center of a 1920 × 1032 Teams window.
- Density: Windows `AppliedDPI = 96` (100%). Source pixels are 1536 × 738; the conceptual sketch region was cropped and normalized to 720 × 207 only for the combined comparison. Implementation captures remain 1:1 native pixels. CSS size and browser device scale factor are not applicable to this native GPUI surface.
- Primary interactions tested: moving the pointer into the collapsed island expanded it after the configured delay; moving it into Teams content collapsed it; the process remained running in both states.
- Console errors: not applicable to the native app. The QA build emitted no runtime failure and all 52 Rust tests passed before capture.

## Full-view comparison

The 720 × 120 crop includes the complete Snapbar surface, the title-bar root, the caption-bottom boundary, and adjacent Teams controls. Both states read as one opaque-black shape growing out of the title bar rather than a floating pill. The 8-pixel drop creates the hanging-island cue without covering the center meeting content or the right-side Teams actions.

A separate focused crop was not needed: the component and all of its type, icons, spacing, corner treatment, and surrounding title-bar context are already legible at 1:1 in the full comparison.

## Required fidelity surfaces

- Fonts and typography: the existing compact Segoe UI-style system text remains readable, vertically centered, and unchanged in weight or wrapping. Expanded status copy fits without truncation.
- Spacing and layout rhythm: collapsed content remains balanced inside 92 × 38 pixels. Expanded controls retain even 6-pixel gaps and a centered 30-pixel action row inside 272 × 38 pixels. The square top and 16-pixel bottom radius provide the requested attached silhouette.
- Colors and visual tokens: the island uses opaque near-black `#050506`; white/gray content and the red capture action retain clear contrast. Status is communicated by both dot color and text.
- Image quality and asset fidelity: Snapbar uses the repository's existing camera, folder, refresh, and power SVG assets. The reference is a conceptual shape sketch and contains no production raster asset to reproduce.
- Copy and content: `Snapbar` and the Japanese readiness label preserve current product meaning. No prompt or design-process copy appears in the product UI.

## Findings

- No actionable P0, P1, or P2 mismatch remains.
- P3 follow-up: a connected width morph could make disclosure feel even closer to Dynamic Island. It is intentionally deferred because the current native window region changes atomically with the visible silhouette, and the user's earlier priority was immediate hover response.

## Comparison history

- Pass 1: compared the source sketch with both live collapsed and expanded captures in `target/design-qa/comparison.png`. No P0/P1/P2 issue was found, so no post-comparison visual fix loop was required.

## Implementation checklist

- [x] Attach the surface to the caption bottom edge.
- [x] Keep the title-bar-wide vertical hover coverage and click-through surroundings.
- [x] Preserve compact, collapsed, and expanded controls.
- [x] Verify collapsed and expanded states in a live Teams meeting.

final result: passed
