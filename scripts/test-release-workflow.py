#!/usr/bin/env python3
"""Assert the fork's intentionally small native-release contract."""

from pathlib import Path
import re


WORKFLOW = Path(__file__).resolve().parents[1] / ".github" / "workflows" / "release.yml"
REPO_ROOT = WORKFLOW.parents[2]


def job_blocks(text: str) -> dict[str, str]:
    jobs = text.index("\njobs:\n") + len("\njobs:\n")
    matches = list(re.finditer(r"(?m)^  ([A-Za-z0-9_-]+):\s*$", text[jobs:]))
    blocks: dict[str, str] = {}
    for index, match in enumerate(matches):
        start = jobs + match.start()
        end = jobs + (matches[index + 1].start() if index + 1 < len(matches) else len(text[jobs:]))
        blocks[match.group(1)] = text[start:end]
    return blocks


def matrix_entries(block: str) -> set[tuple[str, str, str, str]]:
    pattern = re.compile(
        r"(?m)^          - arch: (\S+)\n"
        r"            runner: (\S+)\n"
        r"            target: (\S+)\n"
        r"            asset: (\S+)$"
    )
    return {match.groups() for match in pattern.finditer(block)}


def require(block: str, value: str, context: str) -> None:
    if value not in block:
        raise AssertionError(f"{context} is missing {value!r}")


def main() -> None:
    for retired_path in (
        "install.sh",
        ".github/workflows/docker-release.yml",
        "docs/docker.md",
        "docker/Dockerfile.runtime",
        "scripts/prepare-docker-runtime-context.sh",
        "scripts/test-docker-runtime-context.sh",
        "scripts/generate-release-notes.sh",
        "scripts/test-generate-release-notes.sh",
        "scripts/dispatch-release-event.sh",
        "scripts/test-dispatch-release-event.sh",
    ):
        if (REPO_ROOT / retired_path).exists():
            raise AssertionError(f"retired release consumer still exists: {retired_path}")

    text = WORKFLOW.read_text(encoding="utf-8")
    blocks = job_blocks(text)

    expected_jobs = {"prepare-release", "build-linux", "build-windows"}
    if set(blocks) != expected_jobs:
        raise AssertionError(
            f"release jobs must be exactly {sorted(expected_jobs)}, got {sorted(blocks)}"
        )

    prepare = blocks["prepare-release"]
    require(prepare, "outputs:", "prepare-release")
    require(prepare, "tag:", "prepare-release")
    require(prepare, "version:", "prepare-release")
    require(prepare, "contents: write", "prepare-release")
    require(prepare, "softprops/action-gh-release@", "prepare-release")
    if re.search(r"(?m)^    needs:", prepare):
        raise AssertionError("prepare-release must run before and independently of builds")

    linux = blocks["build-linux"]
    for value in (
        "needs: prepare-release",
        "cache-workspace-crates: true",
        "softprops/action-gh-release@",
        "tag_name: ${{ needs.prepare-release.outputs.tag }}",
        "files: ${{ matrix.asset }}",
    ):
        require(linux, value, "build-linux")
    expected_linux = {
        ("x86_64", "ubuntu-22.04", "x86_64-unknown-linux-gnu", "dist/*.deb"),
        ("aarch64", "ubuntu-22.04-arm", "aarch64-unknown-linux-gnu", "dist/*.tar.gz"),
    }
    if matrix_entries(linux) != expected_linux:
        raise AssertionError(
            f"Linux release matrix must be {sorted(expected_linux)}, "
            f"got {sorted(matrix_entries(linux))}"
        )

    windows = blocks["build-windows"]
    for value in (
        "needs: prepare-release",
        "cache-workspace-crates: true",
        "softprops/action-gh-release@",
        "tag_name: ${{ needs.prepare-release.outputs.tag }}",
        "files: ${{ matrix.asset }}",
    ):
        require(windows, value, "build-windows")
    expected_windows = {
        ("x86_64", "windows-latest", "x86_64-pc-windows-msvc", "dist/*.zip"),
        ("aarch64", "windows-11-arm", "aarch64-pc-windows-msvc", "dist/*.zip"),
    }
    if matrix_entries(windows) != expected_windows:
        raise AssertionError(
            f"Windows release matrix must be {sorted(expected_windows)}, "
            f"got {sorted(matrix_entries(windows))}"
        )
    lto_workaround = windows.find("- name: Disable ThinLTO on Windows aarch64")
    compile_step = windows.find("- name: Compile release binaries")
    if lto_workaround < 0 or lto_workaround > compile_step:
        raise AssertionError("Windows aarch64 must disable ThinLTO before compiling")
    require(
        windows[lto_workaround:compile_step],
        "if: matrix.arch == 'aarch64'",
        "Windows aarch64 ThinLTO workaround",
    )
    require(
        windows[lto_workaround:compile_step],
        'echo "CARGO_PROFILE_RELEASE_LTO=false" >> "$GITHUB_ENV"',
        "Windows aarch64 ThinLTO workaround",
    )

    forbidden = (
        "build-macos",
        "create-release:",
        "publish-docker:",
        "verify-install-script:",
        "verify-macos-install-script:",
        "actions/upload-artifact@",
        "actions/download-artifact@",
        "package-windows-installer.sh",
        "dist/*.AppImage",
        "dist/*.rpm",
        "dist/*.exe",
        "RUST_LOG: info",
    )
    for value in forbidden:
        if value in text:
            raise AssertionError(f"release workflow must not contain {value!r}")

    print("release workflow contract passed")


if __name__ == "__main__":
    main()
