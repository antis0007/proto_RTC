# Desktop GUI Smoke Checklist

Use this quick pass whenever the message list/bubble layout is touched.

## Window sizes
- [ ] Small: ~900x600
- [ ] Medium: ~1280x800
- [ ] Large: ~1600x1000+

## Message pane validation
- [ ] Scroll area reaches the right panel edge; no persistent dead strip.
- [ ] Vertical scrollbar remains on the message-pane boundary at all window sizes.
- [ ] Message rows consume the full pane width while bubbles stay constrained.
- [ ] Bubble padding remains stable (not cramped on small, not overly sparse on large).

## Content checks
- [ ] Plain text messages wrap naturally and never overlap timestamps.
- [ ] Attachment download controls are fully visible after resize.
- [ ] Image attachment previews render without clipping and keep aspect ratio.
- [ ] After opening/closing expanded image preview, message layout remains stable.

## Resize behavior
- [ ] Repeatedly drag narrower/wider and confirm no clipping of controls.
- [ ] Confirm there are no overlapping widgets while previews are loading.
