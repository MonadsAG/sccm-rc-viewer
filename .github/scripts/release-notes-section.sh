#!/usr/bin/env bash
# Print the "## Release notes" section of a PR description read from stdin:
# the lines up to the next "## " heading, without HTML comments, CRs and
# leading/trailing blank lines. Prints nothing when the section is missing
# or empty. Used by pr-release-notes.yml (is it there?) and
# collect-release-notes.sh (what goes into the release).
set -euo pipefail

tr -d '\r' | awk '
  BEGIN { inside = 0; comment = 0 }
  /^##[ \t]+/ {
    h = tolower($0); sub(/^##[ \t]+/, "", h); sub(/[ \t]+$/, "", h)
    if (inside) exit
    if (h == "release notes") { inside = 1; next }
  }
  !inside { next }
  {
    line = $0
    out = ""
    while (length(line) > 0) {
      if (comment) {
        e = index(line, "-->")
        if (e == 0) { line = ""; break }
        line = substr(line, e + 3); comment = 0
      } else {
        s = index(line, "<!--")
        if (s == 0) { out = out line; line = ""; break }
        out = out substr(line, 1, s - 1); line = substr(line, s + 4); comment = 1
      }
    }
    if (out ~ /^[ \t]*$/ && $0 ~ /<!--|-->/) next
    print out
  }
' | awk '
  # trim leading and trailing blank lines
  NF { started = 1; for (i = 0; i < blanks; i++) print ""; blanks = 0; print; next }
  started { blanks++ }
'
