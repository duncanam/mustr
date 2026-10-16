# Dashboard design

The dashboard should read as a coordination console, not three equally weighted empty tables.

## Visual direction

- **Hierarchy before decoration:** a compact identity/navigation area, secondary automation controls, then the selected space's content.
- **Balanced density:** bound participant information to a sidebar on wide terminals; give task descriptions and addressed communication the larger reading area. Stack content when space is constrained.
- **Intentional empty states:** explain what is missing and the next useful action rather than showing empty table headers. Never invent participants, task progress, or messages to fill space.
- **Restrained emphasis:** use an accent for navigation and headings, semantic status colors sparingly, and explicit text labels so meaning survives monochrome rendering.
- **Terminal-native:** inherit the terminal background and foreground. Do not force a dark theme on a light terminal; avoid RGB palettes and dependencies on icon fonts.
- **Honest state:** participant activity is not task completion; sending a message is not evidence that work finished. Presentation must preserve those distinctions.
- **Discoverable controls:** keep keyboard hints visible and detailed help available without changing the established commands.

The [Ratatui showcase](https://ratatui.rs/showcase/apps/) provides useful references: oxker's compact status summaries, counted panel headings and rounded borders; joshuto's bounded columns and strong selection hierarchy. These are composition cues, not themes to copy verbatim. Alignment with herdr should remain terminal-native rather than impose a competing color scheme.

## Review criteria

Inspect both empty and populated fixtures at wide and narrow terminal sizes. Check long labels, help, all automation switches, and no-color output. TestBackend checks should cover readable content and terminal-default background behavior, not only whether drawing avoids a panic.

Synthetic render previews do not replace checking the running application in the user's terminal: font metrics, ANSI palette, dim-text contrast, and terminal background are environment-dependent. Updating the source does not replace or restart an already-running hub.
