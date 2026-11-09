// The Zest components the dashboard draws with.
//
// Only `boot.ts` imports this module. Each Zest class extends `HTMLElement`,
// which a test runner without a browser does not have, so the views make the
// elements by tag name and set attributes, and this module gives each tag its
// class before the first draw.
//
// Each class comes from its own module, not from the package index. The index
// of a Zest family imports each component of the family, and the bundler keeps
// a class it cannot prove is free of side effects: through the indexes, the
// bundle held a slider, a window manager, and a tree view that the page never
// shows, and was 60% larger.

import { defineElements } from "@catalystcommunity/base-components";
import { GroupBox } from "@catalystcommunity/containers/src/group-box.js";
import { StatusBar, StatusPane } from "@catalystcommunity/containers/src/layouts.js";
import { PushButton } from "@catalystcommunity/controls/src/button.js";
import { CheckBox } from "@catalystcommunity/controls/src/check.js";
import { ComboBox } from "@catalystcommunity/controls/src/combo.js";
import { TextField } from "@catalystcommunity/controls/src/text.js";
import { TableView } from "@catalystcommunity/data-views/src/table-view.js";

export function defineDashboardElements(): void {
  defineElements({
    "push-button": PushButton,
    "check-box": CheckBox,
    "text-field": TextField,
    "combo-box": ComboBox,
    "group-box": GroupBox,
    "status-bar": StatusBar,
    "status-pane": StatusPane,
    "table-view": TableView,
  });
}
