// The TallyOwl browser package.
//
// It records semantic application events. It does not record the Document
// Object Model and it does not do session replay. See D11.
//
// It rides the host application's existing connection. It opens none of its
// own and contacts no TallyOwl domain.
export * from "./value.js";
export * from "./capture.js";
export * from "./client.js";
