# Snapbar app icon

Charcoal tile, white capture corners, and a coral title-bar island. The center stays empty so the mark remains legible at notification-area sizes.

- `snapbar.png`: original generated master, 1254 × 1254, RGBA with transparent surroundings.
- `snapbar.ico`: Windows icon containing 16, 20, 24, 32, 40, 48, 64, 128, and 256 px images.
- `snapbar-{16,24,32,48,256}.png`: size previews and reusable PNG exports.

The original alpha and artwork are preserved. Smaller images and the ICO are mechanical Lanczos exports using Pillow. `build.rs` embeds the ICO as resource 101; the notification-area icon reads that resource, and Inno Setup uses the same ICO.

## Generation

Created with the built-in `image_gen` tool. Final prompt:

```text
Use case: logo-brand. Create ONE finished Windows 11 desktop application icon for Snapbar, a lightweight utility that captures only the shared screen region in a Teams meeting, with a small expanding pill control attached to the title bar. The icon should express a capture frame plus the characteristic title-bar island. Square 1024x1024 canvas, actual transparent alpha background outside the icon. Center a nearly black charcoal rounded-square tile, occupying about 88% of the canvas, large smoothly rounded corners, perfectly front-facing. Within it place a bold, simple warm-white viewfinder made of FOUR substantial L-shaped corner brackets around an empty wide horizontal rectangular capture area. At the top-center opening between the upper brackets place one vivid coral-red horizontal capsule/pill, aligned as the central top of the capture frame. The pill is the only color accent and is confidently sized, easy to recognize at 24 px. The center of the viewfinder remains empty charcoal: no camera lens, no letter, no extra symbols. Optical balance, generous inner spacing, thick coherent geometry and gently rounded bracket ends. Premium restrained product icon; mostly flat graphic with only very subtle depth on the charcoal tile, no glossy 3D extrusion. Use charcoal, warm white, and the existing product's camera-button red (approximately #E5484D). No text, no letters, no wordmark, no Microsoft or Teams logo, no gradients on the white mark, no illustration, no checkerboard painted in the image, no mockup, no multiple options, no border around the canvas. Deliver the clean standalone transparent icon, ready for Windows ICO conversion.
```

Windows size reference: [Microsoft app icon construction](https://learn.microsoft.com/ja-jp/windows/apps/design/iconography/app-icon-construction).
