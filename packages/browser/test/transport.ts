// The CSIL transport family reference implementation, from the csilgen project.
//
// npm cannot depend on a subdirectory of a Git repository, and csilgen does not
// publish the transport to a registry yet, so `./tools.sh setup` fetches the
// pinned revision into `.deps/csilgen`. One revision is named in one place:
// `tools/tallyowl_tools/generate.py`. See docs/IMPLEMENTATION_LOG.md L018.
export * from "../../../.deps/csilgen/transports/typescript/src/index.ts";
