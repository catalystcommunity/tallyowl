// Zest publishes JavaScript without type declarations. These declare only the
// names the dashboard imports, from the modules `zest.ts` imports them from.

declare module "@catalystcommunity/base-components" {
  export function defineElements(tags: Record<string, CustomElementConstructor>): void;
}

declare module "@catalystcommunity/controls/src/button.js" {
  export const PushButton: CustomElementConstructor;
}

declare module "@catalystcommunity/controls/src/check.js" {
  export const CheckBox: CustomElementConstructor;
}

declare module "@catalystcommunity/controls/src/combo.js" {
  export const ComboBox: CustomElementConstructor;
}

declare module "@catalystcommunity/controls/src/text.js" {
  export const TextField: CustomElementConstructor;
}

declare module "@catalystcommunity/containers/src/group-box.js" {
  export const GroupBox: CustomElementConstructor;
}

declare module "@catalystcommunity/containers/src/layouts.js" {
  export const StatusBar: CustomElementConstructor;
  export const StatusPane: CustomElementConstructor;
}

declare module "@catalystcommunity/data-views/src/table-view.js" {
  export const TableView: CustomElementConstructor;
}
