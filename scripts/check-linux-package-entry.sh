#!/usr/bin/env bash
#
# Assert that a deb produced by this repository really provides the executable its own
# service/desktop entries call.
#
# Why this exists
# ---------------
# The deb is assembled from a folder (`build.py --package <folder>`, i.e.
# build.py:849 build_deb_from_folder), which copies that folder verbatim into
# /usr/share/rustdesk/ and ships the unit + desktop files under the stock name. The
# /usr/bin/rustdesk entry is NOT part of the package: it is created at install time by
# res/DEBIAN/postinst. So a staged payload whose basename does not match what the control
# script links -- "nervdesk" instead of "rustdesk", which is what the Sciter CI job used to
# stage -- installs cleanly (`dpkg -i` exits 0) and then fails at run time: systemd cannot
# find /usr/bin/rustdesk. Nothing in the pipeline noticed, because `dpkg -c` looks fine.
#
# Usage
# -----
#   scripts/check-linux-package-entry.sh <deb> [<deb> ...]
#
# Exit codes
# ----------
#   0  every referenced /usr/bin entry resolves inside the package
#   1  at least one entry does not resolve (each failure is printed as "FAIL <deb>: ...")
#   2  usage error, or a required tool is missing
#
# What it reads (nothing else): the package contents, via `dpkg-deb -R`, so it needs no
# root, installs nothing and never touches systemd. The install-time behaviour of the
# control scripts (`dpkg -i`, `systemctl start`) is therefore NOT tested here.
#
# How an entry is considered resolved (any one is enough)
# ------------------------------------------------------
#   R0  the payload itself is shipped as /usr/share/rustdesk/<name>
#   R1  a control script links a literal source that exists in the package, e.g.
#       `ln -f -s /usr/share/rustdesk/rustdesk /usr/bin/rustdesk`
#   R2  a control script probes a list of payload names and links whichever exists, e.g.
#       `for _payload in rustdesk nervdesk; do ... ln -f -s "/usr/share/rustdesk/$_payload"
#       /usr/bin/rustdesk ...` and at least one of those names is present in the package
#
# The entries to check are taken from the *packaged* unit/desktop files and from the
# /usr/bin/<name> destinations the control scripts create, so the check follows the package
# rather than a hardcoded name. Only absolute /usr/bin/<name> references are asserted: a
# desktop entry that calls the program by bare name ("Exec=rustdesk %u") is resolved through
# PATH at run time and is deliberately not treated as an entry of this package.

set -uo pipefail

usage() { echo "usage: $0 <deb> [<deb> ...]" >&2; }

if [ "$#" -lt 1 ]; then
    usage
    exit 2
fi

for tool in dpkg-deb mktemp find grep sed sort; do
    command -v "$tool" >/dev/null 2>&1 || { echo "missing required tool: $tool" >&2; exit 2; }
done

fail_rc=0

for deb in "$@"; do
    if [ ! -f "$deb" ]; then
        echo "FAIL $deb: not a file"
        fail_rc=1
        continue
    fi

    work="$(mktemp -d)" || { echo "cannot create a temp dir" >&2; exit 2; }
    if ! dpkg-deb -R "$deb" "$work" 2>/dev/null; then
        echo "FAIL $deb: dpkg-deb -R failed (not a deb?)"
        rm -rf "$work"
        fail_rc=1
        continue
    fi

    share="$work/usr/share/rustdesk"
    problems=()

    # ---- the payload ----------------------------------------------------------------
    # A regular file directly in /usr/share/rustdesk that is not a shared object: the
    # bundled .so files are libraries, not the program the entries run.
    payloads=()
    if [ -d "$share" ]; then
        while IFS= read -r f; do
            [ -n "$f" ] || continue
            case "${f##*/}" in
                *.so|*.so.*) continue ;;
            esac
            payloads+=("${f##*/}")
        done < <(find "$share" -maxdepth 1 -type f 2>/dev/null | sort)
    fi
    if [ "${#payloads[@]}" -eq 0 ]; then
        problems+=("no payload executable in /usr/share/rustdesk (only libraries there?)")
    fi

    # ---- required entries -----------------------------------------------------------
    refs=()
    add_ref() {
        local p="${1%%[[:space:]]*}"
        case "$p" in
            /usr/bin/*) refs+=("${p##*/}") ;;
        esac
    }
    # unit files: ExecStart=<path> [args]
    while IFS= read -r line; do
        case "$line" in
            ExecStart=*) add_ref "${line#ExecStart=}" ;;
        esac
    done < <(grep -rhs '^ExecStart=' \
                "$work/etc" "$share/files" 2>/dev/null; true)
    # desktop files: Exec=<path> [args] (%U etc. is stripped by add_ref)
    while IFS= read -r line; do
        case "$line" in
            Exec=*) add_ref "${line#Exec=}" ;;
        esac
    done < <(grep -rhs '^Exec=' \
                "$work/usr/share/applications" "$share/files" 2>/dev/null; true)

    # control scripts: destinations of literal `ln [-f] [-s] <src> /usr/bin/<name>`
    pkg_scripts=()
    for d in "$work/DEBIAN"; do
        [ -d "$d" ] || continue
        while IFS= read -r f; do
            [ -n "$f" ] || continue
            pkg_scripts+=("$f")
        done < <(find "$d" -maxdepth 1 -type f 2>/dev/null | sort)
    done

    literal_pairs=()   # "<src> <dst>"
    loop_targets=()    # destinations of a `for x in a b; do ... ln ... /usr/bin/<dst>`
    loop_names=()      # the names that loop iterates over
    for s in "${pkg_scripts[@]:-}"; do
        [ -n "$s" ] || continue
        if grep -q 'for[[:space:]]\+[A-Za-z_][A-Za-z0-9_]*[[:space:]]\+in[[:space:]]' "$s" 2>/dev/null; then
            # names iterated by the first payload-probing loop
            local_names="$(grep -o 'for[[:space:]]\+[A-Za-z_][A-Za-z0-9_]*[[:space:]]\+in[[:space:]]\+[^;]*' "$s" \
                            | head -n 1 | sed 's/.*[[:space:]]in[[:space:]]//')"
            # the loop must actually link a payload from /usr/share/rustdesk
            if grep -q '/usr/share/rustdesk/\$' "$s" 2>/dev/null; then
                for n in $local_names; do
                    loop_names+=("$n")
                done
                # only destinations that a link line actually creates
                while IFS= read -r dst; do
                    [ -n "$dst" ] && loop_targets+=("${dst##*/}")
                done < <(grep -E '(^|[^A-Za-z])ln[[:space:]]' "$s" 2>/dev/null \
                            | grep -o '/usr/bin/[A-Za-z0-9._+-]*' | sort -u)
            fi
        fi
        # literal links: `ln ... "<src>" "/usr/bin/<dst>"` or unquoted
        while IFS= read -r pair; do
            [ -n "$pair" ] && literal_pairs+=("$pair")
        done < <(sed -n 's/.*ln[[:space:]][^|]*[[:space:]]"\{0,1\}\([^"[:space:]]*\)"\{0,1\}[[:space:]]\+"\{0,1\}\(\/usr\/bin\/[^"[:space:]]*\)"\{0,1\}.*/\1 \2/p' "$s" 2>/dev/null)
    done
    for t in "${loop_targets[@]:-}"; do
        [ -n "$t" ] && refs+=("$t")
    done

    # unique refs
    uniq_refs=()
    for r in "${refs[@]:-}"; do
        [ -n "$r" ] || continue
        dup=0
        for u in "${uniq_refs[@]:-}"; do
            [ "$u" = "$r" ] && dup=1 && break
        done
        [ "$dup" -eq 0 ] && uniq_refs+=("$r")
    done

    if [ "${#uniq_refs[@]}" -eq 0 ]; then
        problems+=("no /usr/bin entry referenced by the package (no unit/desktop/control script?)")
    fi

    # ---- resolve each entry ---------------------------------------------------------
    for name in "${uniq_refs[@]:-}"; do
        [ -n "$name" ] || continue
        ok=0
        # R0: the payload is shipped under the very name the entry uses
        [ -f "$share/$name" ] || [ -L "$share/$name" ] && ok=1
        # R1: literal link whose source exists inside the package
        if [ "$ok" -eq 0 ]; then
            for pair in "${literal_pairs[@]:-}"; do
                [ -n "$pair" ] || continue
                src="${pair%% *}"
                dst="${pair##* }"
                [ "${dst##*/}" = "$name" ] || continue
                case "$src" in
                    /*) [ -e "$work$src" ] && ok=1 ;;
                    *)  [ -e "$work/$src" ] && ok=1 ;;
                esac
                [ "$ok" -eq 1 ] && break
            done
        fi
        # R2: the control script probes a list of payload names and links whichever exists
        if [ "$ok" -eq 0 ]; then
            for t in "${loop_targets[@]:-}"; do
                [ "$t" = "$name" ] || continue
                for n in "${loop_names[@]:-}"; do
                    [ -n "$n" ] || continue
                    if [ -e "$share/$n" ]; then ok=1; break 2; fi
                done
            done
        fi
        if [ "$ok" -eq 0 ]; then
            problems+=("entry '$name' does not resolve: /usr/share/rustdesk/$name is absent, no control script links it from an existing payload")
        fi
    done

    if [ "${#problems[@]}" -eq 0 ]; then
        echo "OK   $deb: payload [${payloads[*]}] provides [${uniq_refs[*]}]"
    else
        echo "FAIL $deb"
        for p in "${problems[@]}"; do
            echo "     - $p"
        done
        fail_rc=1
    fi

    rm -rf "$work"
done

exit "$fail_rc"
