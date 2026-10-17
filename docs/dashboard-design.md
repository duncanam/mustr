# Dashboard design

The dashboard should read as a coordination console, not three equally weighted empty tables.

## Visual direction

- **Visible section boundaries:** like Lazygit, use complete thin, rounded outlines with titles embedded in the top border and narrow gutters between panels. Top rules alone are not sufficient: Participants and the shared work pane must remain visibly separate even when sparse. Tasks and Messages are alternate tabs within the work pane, not vertically competing panels. Use terminal-default foreground with DIM for the outlines, normal-contrast bold titles, and one column of interior horizontal padding. Do not dim the entire panel or force a gray that only works on one background.
- **Header hierarchy:** separate branding, space navigation, and automation with blank rows at normal terminal heights. Use wider, faint separators between space tabs and between automation controls. Distinguish bold labels (`Spaces`, `Automation`, `TODO:`), dim shortcut hints (`[t]`) and metric labels, and normal-contrast reported values (`off`, `2/5`). Enabled values use green, not the navigation accent. Reserve stable value widths so toggling a mode does not shift controls or panels. Narrow screens wrap controls; short screens reduce spacing, and help omits the automation strip to leave room for instructions.
- **Balanced density:** bound participant information to a sidebar on wide terminals; give the active Tasks/Messages tab the entire larger reading area. Stack Participants above the shared work pane when space is constrained.
- **Work tabs and counts:** default to Messages; `f` switches between Messages and Tasks without resetting either view's scroll position. Keep both tab labels and the selected space's task total/accepted-completion count visible, including while reading Messages. Accepted tasks count as completed; awaiting-review and cancelled tasks do not. Use compact, unambiguous labels on narrow screens and an active-tab marker that also works without color.
- **Stable browsing:** arrows up/down and `j`/`k` navigate the active work tab; preserve existing space navigation (`Tab`, `Shift-Tab`, horizontal arrows, `h`/`l`) and automation controls. Task selection is anchored to task ID across updates/reordering, is kept in view, and confers no ownership or task action. Clamp/reconcile browsing on resize, removal, and space changes. Show reachable overflow with a range/count rather than telling the user to enlarge the terminal.
- **Intentional empty states:** explain what is missing and the next useful action rather than showing empty table headers. Never invent participants, task progress, or messages to fill space.
- **Restrained emphasis:** use an accent for navigation and headings, semantic status colors sparingly, and explicit text labels so meaning survives monochrome rendering.
- **Terminal-native:** inherit the terminal background and foreground. Do not force a dark theme on a light terminal; avoid RGB palettes and dependencies on icon fonts.
- **Honest state:** participant activity is not task completion; sending a message is not evidence that work finished. Presentation must preserve those distinctions.
- **Discoverable controls:** keep keyboard hints visible and detailed help available without changing the established commands.

The [Ratatui showcase](https://ratatui.rs/showcase/apps/) provides useful references: oxker's compact status summaries, counted panel headings and rounded borders; joshuto's bounded columns and strong selection hierarchy. These are composition cues, not themes to copy verbatim. Alignment with herdr should remain terminal-native rather than impose a competing color scheme.

## Review criteria

Inspect both empty and populated fixtures for each work tab at wide, narrow, and short terminal sizes. Verify independent scrolling, last-task reachability, selection stability under updates, per-space completion counts, and tab navigation without changing space/automation controls. Check long labels, help, all automation switches, and no-color output. TestBackend checks cover readable content, continuous borders on all four sides, dim border versus normal-contrast title styling, content containment, and terminal-default background behavior—not only whether drawing avoids a panic. Account for both border rows in content-sized panels, and calculate wrapping/scroll limits from the bordered inner rectangle.

Synthetic render previews do not replace checking the running application in the user's terminal: font metrics, ANSI palette, dim-text contrast, and terminal background are environment-dependent. Updating the source does not replace or restart an already-running hub.
