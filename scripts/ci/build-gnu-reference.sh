#!/usr/bin/env bash
set -euo pipefail

readonly repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly manifest="$repo_root/parity-reference.toml"
readonly archive_name="gnu-emacs-31.0.90-linux-x86_64.tar.zst"
readonly source_repository="https://github.com/emacs-mirror/emacs.git"
readonly build_root="$(mktemp -d)"
readonly source_root="$build_root/emacs"
readonly stage_root="$build_root/stage"
readonly archive_path="${GNU_REFERENCE_ARCHIVE:-$repo_root/$archive_name}"
readonly checksum_path="${archive_path}.sha256"
readonly verify_root="$build_root/verify"

cleanup() {
    rm -rf "$build_root"
}
trap cleanup EXIT

for command in git make gcc autoconf tar zstd strip file sha256sum xvfb-run timeout; do
    command -v "$command" >/dev/null 2>&1 ||
        { echo "GNU reference build requires '$command'" >&2; exit 1; }
done

mirror_commit="$(
    sed -n 's/^mirror_commit = "\([0-9a-f]\{40\}\)"$/\1/p' "$manifest"
)"
if [[ ! "$mirror_commit" =~ ^[0-9a-f]{40}$ ]]; then
    echo "could not read one valid mirror_commit from $manifest" >&2
    exit 1
fi

git clone --filter=blob:none --no-checkout "$source_repository" "$source_root"
git -C "$source_root" checkout --detach "$mirror_commit"

pushd "$source_root" >/dev/null
./autogen.sh
./configure \
    --without-native-compilation \
    --with-x-toolkit=gtk3 \
    --with-json \
    --with-tree-sitter \
    --with-sqlite3 \
    --without-mailutils
make -j"$(nproc)"
popd >/dev/null

while IFS= read -r -d '' binary; do
    if file "$binary" | grep -q 'ELF'; then
        strip --strip-unneeded "$binary"
    fi
done < <(find "$source_root/src" -maxdepth 1 -type f -perm -u+x -print0)

test -x "$source_root/src/emacs"
test -f "$source_root/src/emacs.pdmp"
for directory in lisp etc lib-src info; do
    test -d "$source_root/$directory"
done

rm -rf "$stage_root" "$archive_path"
mkdir -p "$stage_root/src" "$(dirname "$archive_path")"
cp -a "$source_root/src/emacs" "$stage_root/src/emacs"
cp -a "$source_root/src/emacs.pdmp" "$stage_root/src/emacs.pdmp"
for directory in lisp etc lib-src info; do
    cp -a "$source_root/$directory" "$stage_root/$directory"
done

tar --zstd -cf "$archive_path" -C "$stage_root" \
    src/emacs src/emacs.pdmp lisp etc lib-src info
test -s "$archive_path"
(
    cd "$(dirname "$archive_path")"
    sha256sum "$(basename "$archive_path")" >"$(basename "$checksum_path")"
)
test -s "$checksum_path"

rm -rf "$verify_root"
mkdir -p "$verify_root"
tar --zstd -xf "$archive_path" -C "$verify_root"
readonly verify_emacs="$verify_root/src/emacs"
test -x "$verify_emacs"
test -f "$verify_emacs.pdmp"
for directory in lisp etc lib-src info; do
    test -d "$verify_root/$directory"
done

env \
    "EMACSDATA=$verify_root/etc" \
    "EMACSDOC=$verify_root/etc" \
    "EMACSPATH=$verify_root/lib-src" \
    "EMACSLOADPATH=$verify_root/lisp" \
    "$verify_emacs" --batch -Q --eval '
      (let* ((root (file-name-as-directory
                    (file-name-directory
                     (directory-file-name (getenv "EMACSDATA")))))
             (data (expand-file-name data-directory))
             (doc (expand-file-name doc-directory))
             (standard (locate-library "subr")))
        (dolist (pair `(("data-directory" . ,data)
                        ("doc-directory" . ,doc)
                        ("standard-library" . ,standard)))
          (unless (and (cdr pair)
                       (string-prefix-p root (expand-file-name (cdr pair))))
            (error "%s escaped extracted GNU tree: %S" (car pair) (cdr pair))))
        (kill-emacs 0))'

env \
    "EMACSDATA=$verify_root/etc" \
    "EMACSDOC=$verify_root/etc" \
    "EMACSPATH=$verify_root/lib-src" \
    "EMACSLOADPATH=$verify_root/lisp" \
    timeout --signal=TERM 60s xvfb-run --auto-servernum \
    --server-args="-screen 0 1024x768x24" \
    "$verify_emacs" -Q \
    --eval '(progn (unless (display-graphic-p) (error "GNU GUI did not start")) (kill-emacs 0))'

sha256sum "$archive_path"
