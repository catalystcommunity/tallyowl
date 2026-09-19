# The TallyOwl service image: the head and the collector, in one image.
#
# Both charts run this image and select the binary with `command`, because the
# two services share every library they link and an operator upgrades them
# together. One image is one tag to build, one tag to pull, and one tag for the
# charts to ask for: `appVersion`, which `./tools.sh version set` writes.
#
# The image compiles from source rather than copying a binary from a build
# machine, because a binary built against another machine's libraries is not
# the binary the chart runs.
#
# `./tools.sh release image` builds it. See docs/CI-CD.md section 6.

# The dashboard bundle. The head serves it, so a deployed head without it
# serves an empty page — which is what the first version of this image did.
#
# TypeScript rather than machine code, so this stage compiles rather than
# links: none of the library concerns that make the Rust stage build from
# source apply, but building it here keeps the image self-contained.
FROM docker.io/library/node:26-bookworm AS dashboard

WORKDIR /src
COPY packages/dashboard packages/dashboard
COPY generated/typescript generated/typescript
# The CSIL transport the dashboard imports by relative path. `./tools.sh deps`
# puts it here, and `.dockerignore` lets this one directory through.
COPY .deps/csilgen/transports/typescript .deps/csilgen/transports/typescript

WORKDIR /src/packages/dashboard
RUN npm install --no-audit --no-fund --silent && npm run build

FROM docker.io/library/rust:1.97-bookworm AS build

WORKDIR /src
COPY . .

# `--locked` refuses to move Cargo.lock. A release image builds the dependency
# versions the tests ran against, or it does not build.
RUN cargo build --release --locked --bin tallyowl-head --bin tallyowl-collector

FROM docker.io/library/debian:bookworm-slim

# `ca-certificates` is for the one outbound connection TallyOwl makes to
# something it does not own: an alert webhook (L142). Every other hop verifies
# against the installation's own authority.
RUN apt-get update \
 && apt-get install --yes --no-install-recommends ca-certificates \
 && rm --recursive --force /var/lib/apt/lists/*

# A fixed identity that is not root. Both charts set the same number as
# `fsGroup`, so a mounted volume is writable without a root container.
RUN groupadd --gid 65532 tallyowl \
 && useradd --uid 65532 --gid 65532 --home-dir /var/lib/tallyowl --no-create-home tallyowl \
 && install --directory --owner 65532 --group 65532 /var/lib/tallyowl /etc/tallyowl

COPY --from=build /src/target/release/tallyowl-head /usr/local/bin/tallyowl-head
COPY --from=build /src/target/release/tallyowl-collector /usr/local/bin/tallyowl-collector

# The dashboard bundle, at a fixed path the charts point `dashboard.assets` at.
# The head serves `/assets/<path>` from inside this directory and refuses a
# path that walks out of it.
COPY --from=dashboard /src/packages/dashboard/dist /usr/local/share/tallyowl/dashboard

USER 65532:65532
WORKDIR /var/lib/tallyowl

# There is no default service. Each chart names the binary it runs, and an
# image that starts a service by itself starts the wrong one half the time.
CMD ["tallyowl-head", "--help"]
