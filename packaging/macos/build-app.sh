#!/usr/bin/env bash
# Assembles SpotFreeze.app from a compiled spotfreeze binary and ad-hoc signs it.
#
# Usage: build-app.sh <path-to-spotfreeze-binary> <version> [output-dir]
#
# Prints the path to the assembled .app on stdout. Runs on macOS only
# (requires codesign).

set -euo pipefail

readonly APP_NAME="SpotFreeze"
readonly BUNDLE_EXECUTABLE="spotfreeze"

usage() {
  echo "Usage: $0 <path-to-spotfreeze-binary> <version> [output-dir]" >&2
}

main() {
  local binary_path="${1:-}"
  local version="${2:-}"
  local output_dir="${3:-.}"
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  readonly script_dir

  if [[ -z "${binary_path}" || -z "${version}" ]]; then
    usage
    return 2
  fi
  if [[ ! -f "${binary_path}" ]]; then
    echo "error: binary not found: ${binary_path}" >&2
    return 1
  fi
  if [[ ! "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
    echo "error: version must be a plain semver (e.g. 1.2.3), got: ${version}" >&2
    return 2
  fi

  local app_dir="${output_dir%/}/${APP_NAME}.app"
  local contents_dir="${app_dir}/Contents"

  rm -rf "${app_dir}"
  mkdir -p "${contents_dir}/MacOS"
  install -m 0755 "${binary_path}" "${contents_dir}/MacOS/${BUNDLE_EXECUTABLE}"
  sed "s/@VERSION@/${version}/g" "${script_dir}/Info.plist" > "${contents_dir}/Info.plist"

  codesign --force --sign - "${app_dir}"

  echo "${app_dir}"
}

main "$@"
