// Zest ships its declarations at each package index only. `zest.ts` imports
// each class from its own module (see the note there), so each of those
// modules takes its declaration from the index of its package.
//
// Do not declare a package index here. An ambient declaration of
// `@catalystcommunity/<package>` hides the declarations that Zest ships.

declare module "@catalystcommunity/controls/src/button.js" {
  export { PushButton } from "@catalystcommunity/controls";
}

declare module "@catalystcommunity/controls/src/check.js" {
  export { CheckBox } from "@catalystcommunity/controls";
}

declare module "@catalystcommunity/controls/src/combo.js" {
  export { ComboBox } from "@catalystcommunity/controls";
}

declare module "@catalystcommunity/controls/src/text.js" {
  export { TextField } from "@catalystcommunity/controls";
}

declare module "@catalystcommunity/containers/src/group-box.js" {
  export { GroupBox } from "@catalystcommunity/containers";
}

declare module "@catalystcommunity/containers/src/layouts.js" {
  export { StatusBar, StatusPane } from "@catalystcommunity/containers";
}

declare module "@catalystcommunity/data-views/src/table-view.js" {
  export { TableView } from "@catalystcommunity/data-views";
}
