"""The verbs. `tools.sh` calls this, and so does every Reactorcide job.

Keep the dispatch here and the work in a module. A verb that grows an option is
still one line in this table.
"""

from __future__ import annotations

import sys

from . import audit, build, commits, deps, dev, drill, generate, helm, packages, release, soak
from .commands import ToolFailed, warn

USAGE = """\
tools.sh <verb>

  setup                   Fetch what is needed, generate from csil/, and write a
                          local configuration file from the example
  gen                     Generate the clients from csil/
  gen-check               Generate into a temporary directory and fail on drift
  csil-validate           Check every specification
  build                   Build every service
  test                    Run every test, in every maintained language
  test-rust               Run the Rust tests only
  test-go                 Run the Go tests only
  test-ts                 Run the TypeScript tests only
  test-tools              Run the tooling's own tests
  golden                  Rewrite golden/vectors.json from the Rust side
  deps                    Fetch the pinned dependencies a package manager cannot
  deps show               Say which csilgen release this would install from,
                          and which asset it would take for each artifact
  fmt                     Format the Rust code
  lint                    Check formatting and run the linter
  check                   csil-validate, fmt, lint, and test
  helm-check              Lint the charts, render every profile, and prove
                          every refusal refuses
  audit                   Check dependencies: advisories, licenses, sources

  commits check [<base>]  Fail when a commit does not say what kind of change
                          it is. The version is computed from that

  version                 Say what version this working tree is
  version set <version>   Write one version over every version site and
                          regenerate the clients
  version check           Fail when two version sites disagree
  release plan            Say what the next release would be, from the
                          conventional commits since the last tag
  release stamp           Bring the tree up to date and write the next version
                          into it. Commits nothing: the release job does this
                          before it builds anything
  release tag             Commit the stamped version, tag it, and push. The
                          last step of a release, after every artifact exists
  release package         Build every publishable artifact: both charts, the
                          browser package, and the service image
  release image           Build the service container image only
  release crates-plan     Say which crate names a crates.io publication takes,
                          in the order the pushes have to happen
  release check-tag <tag> Fail when a release tag is not this version

  drill dr                Run the disaster-recovery drill: back up, destroy,
                          restore, rebuild, and measure every step
  drill overload          Offer far more than the path commits, and prove the
                          bounds: backpressure, memory, disk, and the drain

  soak up [--rate N]      Start the cross-cluster soak: three heads, two
                          collectors, sustained load, and the monitor
  soak status             Say how the soak is going
  soak report             Summarize the journal and the reconciliation
  soak down               Stop the whole soak and check for survivors

  dev up                  Start Corndogs, the head, and the collector
  dev up --without head   Start the rest, so a debugger owns the head
  dev down                Stop them and leave the data directory
  dev reset               Stop them and remove the data directory
  dev logs                Follow the logs
  dev status              Say what is running
  config check            Resolve the configuration and say where each value came from
"""


def main(argv: list[str]) -> int:
    if not argv or argv[0] in ("-h", "--help", "help"):
        print(USAGE)
        return 0

    verb, rest = argv[0], argv[1:]

    if verb == "setup":
        return build.setup()
    if verb == "gen":
        generate.validate()
        generate.generate()
        return 0
    if verb == "gen-check":
        generate.check_drift()
        return 0
    if verb == "csil-validate":
        generate.validate()
        return 0
    if verb == "build":
        return build.build()
    if verb == "test":
        return build.test()
    if verb == "test-rust":
        return build.test_rust()
    if verb == "test-go":
        return packages.go_test()
    if verb == "test-ts":
        return packages.typescript_test()
    if verb == "test-tools":
        return build.test_tools()
    if verb == "golden":
        return build.golden()
    if verb == "deps":
        if rest[:1] == ["show"]:
            return deps.show()
        deps.fetch_csilgen_binary()
        deps.fetch_csilgen()
        deps.fetch_duckdb()
        deps.fetch_helm()
        deps.fetch_node()
        deps.fetch_semver_tags()
        deps.fetch_crane()
        deps.fetch_gh()
        return packages.typescript_install()
    if verb == "fmt":
        return build.format_code()
    if verb == "lint":
        return build.lint()
    if verb == "check":
        return build.check()
    if verb == "helm-check":
        return helm.check()
    if verb == "audit":
        return audit.audit()

    if verb == "commits":
        if rest[:1] == ["check"]:
            return commits.check(rest[1] if len(rest) > 1 else None)
        warn("The only commit verb is `commits check [<base>]`.")
        return 1

    if verb == "version":
        return _version(rest)

    if verb == "release":
        return _release(rest)

    if verb == "dev":
        return _dev(rest)

    if verb == "soak":
        return _soak(rest)

    if verb == "drill":
        if rest[:1] == ["dr"]:
            return drill.dr()
        if rest[:1] == ["overload"]:
            return drill.overload()
        warn("The drills are `drill dr` and `drill overload`.")
        return 1

    if verb == "config":
        if rest[:1] == ["check"]:
            return dev.config_check()
        warn("The only configuration verb is `config check`.")
        return 1

    warn(f"`{verb}` is not a verb this repository has.")
    print(USAGE)
    return 1


def _version(argv: list[str]) -> int:
    if not argv:
        print(release.current_version())
        return 0
    action, rest = argv[0], argv[1:]

    if action == "check":
        return release.check()
    if action == "set":
        if len(rest) != 1:
            warn("`version set` takes one version, such as `0.1.0-rc.1`.")
            return 1
        return release.set_version(rest[0])

    warn(f"`version {action}` is not a verb this repository has.")
    return 1


def _release(argv: list[str]) -> int:
    if not argv:
        warn("`release` needs one of: plan, stamp, package, tag, image, crates-plan, check-tag.")
        return 1
    action, rest = argv[0], argv[1:]

    if action == "check-tag":
        if len(rest) != 1:
            warn("`release check-tag` takes one tag, such as `v0.1.0-rc.1`.")
            return 1
        return release.check_tag(rest[0])
    if action == "crates-plan":
        return release.crates_report()
    if action == "image":
        return release.build_image(rest[0] if rest else None)
    if action == "package":
        return release.package()
    if action == "plan":
        return release.plan_report()
    if action == "stamp":
        return release.stamp_release()
    if action == "tag":
        return release.tag_release()

    warn(f"`release {action}` is not a verb this repository has.")
    return 1


def _soak(argv: list[str]) -> int:
    if not argv:
        warn("`soak` needs one of: up, down, status, report, monitor.")
        return 1
    action, rest = argv[0], argv[1:]

    if action == "up":
        rate = 500
        index = 0
        while index < len(rest):
            if rest[index] == "--rate" and index + 1 < len(rest):
                rate = int(rest[index + 1])
                index += 2
                continue
            if rest[index].startswith("--rate="):
                rate = int(rest[index].split("=", 1)[1])
                index += 1
                continue
            warn(f"`{rest[index]}` is not an option `soak up` takes.")
            return 1
        return soak.up(rate=rate)

    if action == "down":
        return soak.down()
    if action == "status":
        return soak.status()
    if action == "report":
        return soak.report()
    if action == "roll":
        return soak.roll()
    if action == "monitor":
        return soak.monitor()

    warn(f"`soak {action}` is not a verb this repository has.")
    return 1


def _dev(argv: list[str]) -> int:
    if not argv:
        warn("`dev` needs one of: up, down, reset, logs, status.")
        return 1
    action, rest = argv[0], argv[1:]

    if action == "up":
        without: list[str] = []
        index = 0
        while index < len(rest):
            if rest[index] == "--without" and index + 1 < len(rest):
                without.append(rest[index + 1])
                index += 2
                continue
            if rest[index].startswith("--without="):
                without.append(rest[index].split("=", 1)[1])
                index += 1
                continue
            warn(f"`{rest[index]}` is not an option `dev up` takes.")
            return 1
        return dev.up(without=without)

    if action == "down":
        return dev.down(remove_data=False)
    if action == "reset":
        return dev.down(remove_data=True)
    if action == "logs":
        return dev.logs()
    if action == "status":
        return dev.status()

    warn(f"`dev {action}` is not a verb this repository has.")
    return 1


def entry() -> int:
    try:
        return main(sys.argv[1:])
    except ToolFailed as failure:
        warn(str(failure))
        return failure.exit_code
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    raise SystemExit(entry())
