# Desktop GUI Smoke Checklist

Use this quick pass whenever the message list/bubble layout is touched.

## Visual Overhaul Acceptance

Run this section for any styling/layout refresh intended to align with modern chat UX patterns (Discord/Slack/RTC quality bar).

### Scope guard (concurrency-safe)
- [ ] This task edits checklist/documentation only.
- [ ] No source-code files changed in this task, so implementation work can proceed concurrently without merge conflicts.

### Implementation sequencing (recommended merge order)
- [ ] Merge in sequence: **(1) login visual pass**, **(2) top controls + global spacing/layout**, **(3) guild/channel list theming + interaction states**, **(4) final acceptance/regression pass**.

### Login page acceptance
- [ ] Input fields, helper text, and placeholders have clear contrast against backgrounds (comfortable readability at a glance).
- [ ] Credential entry and invite/join affordances are visually distinct sections (spacing, divider, grouping, or card structure).
- [ ] Professional hierarchy is obvious: app icon/branding -> title -> subtitle/supporting copy -> primary CTA.
- [ ] Primary CTA and secondary actions have clear emphasis and are easy to discover without visual clutter.
- [ ] Focus/hover/disabled/error states are visible and consistent with the global theme.
- [ ] Responsive behavior verified at small/medium/large windows with no clipped labels, collapsed controls, or unusable hit targets.

### Main workspace acceptance
- [ ] Consistent margins/padding around all major labels and titles, including `Server`, `Username`, `Invite`, connection/status text, `Guilds`, `Channels`, `Messages`, and equivalent section headers.
- [ ] Top action controls are aligned on a clear baseline, have consistent spacing, and remain usable at all target widths.
- [ ] Guild and channel rows consume available row width (no awkward dead zones) while preserving readable content padding.
- [ ] Active, hover, and selected states for guild/channel rows are visually consistent and clearly distinguishable.
- [ ] Color usage is consistent across guilds/channels/buttons and follows the target scheme (primary, secondary, success, warning, danger, neutral roles).
- [ ] Sidebar, top bar, and message pane maintain balanced visual density (no overcrowded/overly sparse regions).
- [ ] Typography scale is consistent (header/subheader/body/meta) and supports quick scan of navigation and chat content.
- [ ] Interactive controls meet practical hit target expectations and remain easy to click in dense layouts.

### Regression checks for visual overhaul
- [ ] No command behavior changes for sign-in/auth, refresh, invite/join, and guild/channel selection flows.
- [ ] Keyboard navigation and focus order still work across login, top actions, guild list, and channel list.
- [ ] Narrow-window layouts show no text clipping, label collisions, or truncated status badges.
- [ ] Existing message and attachment interactions still behave the same after visual-only updates.
- [ ] Theme contrast remains acceptable in all key surfaces (sidebars, cards, buttons, list rows, status text).

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
