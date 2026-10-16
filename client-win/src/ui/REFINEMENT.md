# UI refinement against the mockups (2026-10-14)

Each `--ui-demo` state (src/mock.rs) was compared with the HTML mockups in
`client-win/mockups/` at 1100x720 and at the 760x520 shell minimum. The
layout contract that the tests check is in `contract.rs`; the tests are in
`client-win/tests/ui_layout.rs`.

Status: **fixed** (changed in code) or **kept** (left on purpose, with the
reason).

## Owner's bugs

1. **fixed** Your PCs rows: the sub-line (ID, last seen, mode) was drawn below
   the row's card. Each row is now a fixed 64 px box with two computed 22 px
   lines centred in it (`row_lines`); the name line truncates to leave room
   for its pill.
2. **fixed** "1 viewer connected" hung below the This PC row in the sidebar.
   It is now the row's own sub-line, shortened ("1 viewer · admin").
3. **fixed** This PC header: the pill was clipped ("…ning") and status,
   viewers and PC ID ran together on one line. The header is now a title line
   (title left, Restart/Stop right) and two labelled fields in a fixed 56 px
   key column: `Status` (pill, then the viewer line, which may truncate) and
   `PC ID`.

## Rhythm and grid

4. **fixed** egui's `interact_size.y` was 36, which made every horizontal
   row, grid row and wrapped line at least 36 px tall: settings rows, vouch
   facts, stats rows, monitor rows and switches were spread out against the
   mockups' 18-22 px lines. It is now `theme::LINE_H` (20); controls set their
   own heights.
5. **fixed** Small buttons were 28 px; now 32 (`CONTROL_SM_H`), the mockup's
   compact button and the tests' minimum target.
6. **fixed** The This PC divider and body sat 6/16 px under the header; now
   12/24 (4 px grid).
7. **fixed** This PC columns: gap 26 → 28; both column heads are 32 px rows so
   "Hosting settings" and "Paired devices" start level.
8. **fixed** Setting rows: 8 px above and below, label and switch on one 20 px
   line, help text 2 px under it, wrapped short of the switch column.
9. **fixed** Paired device rows: name line 32 px (room for Revoke), sub-line
   20 px; the name truncates before the pill and the "vouched by" tag; the
   controls are measured, so nothing overlaps at the minimum width.
10. **fixed** Connecting / stream failed: the step list sat 8 px under the
    lead; now 20, as in the mockup.

## Alignment and width

11. **fixed** Panel main buttons (Connect, Numbers match, Deny) were natural
    width inside justified layouts; they now fill the column like the
    mockups' block buttons.
12. **fixed** Notices did not fill the pane when their text was short (09,
    12); they now take the full width.
13. **fixed** Settings: the key file path ran into the help column. The rows
    follow the mockup's `1fr auto` grid: the control is laid out first at its
    natural width and the text column takes the rest; the path may truncate
    (full path on hover).
14. **fixed** This PC at the minimum window: the two columns were cramped;
    below 600 px of body width the devices go under the settings.
15. **fixed** Host footer ("Host build …", Roll back, Open logs) overlapped
    at the minimum width; the build line now truncates before the buttons,
    and the footer row has 8 px above it under the divider.
16. **fixed** Session strip at the minimum window ran past the window edge.
    Below 1000 px the strip goes compact (no release chord in the capture
    group, quality as an icon only).
17. **fixed** Mode: the strip had Work and Game as two buttons; the mockup has
    one button with the current mode and `chevrons-up-down`. A click switches
    to the other mode.

## Type and text

18. **fixed** Vouch panel: "Request expires in 8:41 ." had a gap before the
    full stop (three labels). It is one paragraph with the time as a mono run.
19. **fixed** "Different case: PC key changed." notice: the bold lead and the
    body were two labels and wrapped as two blocks; one paragraph now.
20. **fixed** Draft tags inside notices (key rotation) sat off the text
    baseline and pushed the first line down. Designer call: the tag sits on
    its own line above the text, 6 px gap (the mockup has it inline, which
    egui cannot wrap around).
21. **fixed** Footer key note ("Key stored with Windows data protection") and
    other texts that are meant to give way are marked as allowed to truncate;
    every other elided text is a test failure.

## Colour, shape and states

22. **fixed** Reconnecting card: the paw accent was a square bar that poked
    past the card's rounded left corners; it now follows them.
23. **fixed** Approval wait: the pulse ring was a circle; the mockup's ring is
    the rounded square of the icon box, 6 px out.
24. **kept** Hover and selected states of rows, buttons, monitor rows and
    switches already match the mockups (fills and the apricot marker); no
    change.

## Kept on purpose

25. **kept** Stats panel: the mockup also shows "(0.1%)" after dropped,
    "packet loss" and "present". The model's `Stats` has no fields for them
    and it is filled by the media lane; adding them belongs there.
26. **kept** Monitor picker: the mockup has a "Follow active window" row.
    There is no action or backend for it yet; drawing a dead switch would
    promise a feature.
27. **kept** Pair-enter fields are empty in the demo, the mockup shows them
    filled: the demo shows the real first-open state.
28. **kept** Titlebar glyphs are painted lines (the caption buttons have no
    Lucide equivalent that matches Windows); left as they were, compared
    only by eye in the shots.

## Found at the 760x520 minimum (no mockup; designer judgement)

29. **fixed** Your PCs list below 480 px: the Connect button became a 36 px
    Lucide `monitor-play` icon button (label as tooltip and accessible name),
    so the name and ID lines keep their width.
30. **fixed** A PC with a pairing request in the narrow list: the
    "1 pairing request" pill moves to the name line in place of
    Online/Offline instead of squeezing the ID line to 84 px.
31. **fixed** Session strip below 1000 px: chips and the mode/quality buttons
    drop their labels (icon plus value, details on hover), so the strip fits.
32. **fixed** Approval rail at short heights: the rail body scrolls instead of
    pushing the steps past the window bottom; the vouch panel scrolls too.
33. **fixed** Flow pane titles wrap, with the Draft tag under the title when
    both do not fit; the pane width is measured outside its scroll area, so
    content no longer grows past the window.
34. **fixed** Pane button rows (stream failed: three buttons; key refused:
    two) wrap onto a second line in a narrow pane instead of running past it.
35. **fixed** Key rotation card: Old ID and New ID stack below 380 px (no
    Lucide down arrow in the set, so no arrow when stacked).
36. **fixed** Settings capped its column with `set_max_width(620)`, which
    widened it past a narrow pane; it is now the smaller of the two.
37. **fixed** Settings rows stack the control under the text below 460 px;
    the key path truncates with an ellipsis.
38. **fixed** Tests: a run-on rule (more than three texts on one line with
    gaps under 16 px) catches the old This PC header; scroll bars are left out
    of the overlap check and items are clipped to their scroll viewport before
    the window check.
39. **fixed** This PC: the sidebar list and the host body were scroll areas
    with the same id; egui painted "First use / Second use of ScrollArea ID"
    over both in debug builds and they shared a scroll offset. Every
    contract scroll is now salted with its name (the sidebar list too), and
    the tests fail when that warning is painted.
