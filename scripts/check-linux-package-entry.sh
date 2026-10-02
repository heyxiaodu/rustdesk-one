#!/usr/bin/env bash
#
# Assert that a Linux package produced by this repository really provides the executable its
# own service/desktop entries call.
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
#   scripts/check-linux-package-entry.sh --rpm [--require-tools] <deb|rpm|spec> [...]
#
#   --rpm            also accept .rpm artefacts and .spec files. A .deb is still accepted.
#   --require-tools  fail (exit 2) when a layer's tools are missing, instead of skipping that
#                    layer with exit 3. CI must pass this: a silent skip is a check that never
#                    runs, and "not checked" must never read as "checked and fine".
#
# Exit codes
# ----------
#   0  every referenced /usr/bin entry resolves inside the package
#   1  at least one input does not resolve (each failure is printed as "FAIL <input>: ...")
#   2  usage error, unknown option, a .rpm/.spec input without --rpm, or a required tool is
#      missing while --require-tools is in effect
#   3  nothing failed, but at least one layer was skipped because its tools are missing. Only
#      reachable without --require-tools, and never reported as a pass.
#
# What it reads (nothing else): `dpkg-deb -R` for debs, `rpm2cpio | cpio` plus
# `rpm -qp --scripts` for rpms, plain text for specs. It needs no root, installs nothing and
# never touches systemd. The install-time behaviour of the control scripts (`dpkg -i`,
# `rpm -i`, `systemctl start`) is therefore NOT tested here.
#
# How an entry is considered resolved (any one is enough)
# ------------------------------------------------------
#   R0  the payload itself is shipped as /usr/share/rustdesk/<name>, as a regular file or as a
#       symlink that actually resolves. `-e` follows symlinks deliberately: the earlier
#       `-f || -L` test accepted a *dangling* symlink, which installs cleanly and still leaves
#       the entry dead -- the exact failure this script exists to catch.
#   R1  a control script links a literal source that exists in the package, e.g.
#       `ln -f -s /usr/share/rustdesk/rustdesk /usr/bin/rustdesk`
#   R2  a control script probes a list of payload names and links whichever exists, e.g.
#       `for _payload in rustdesk nervdesk; do ... ln -f -s "/usr/share/rustdesk/$_payload"
#       /usr/bin/rustdesk ...` and at least one of those names is present in the package
#   R3  (.rpm only) the package installs the file itself at /usr/bin/<name>, as res/rpm.spec
#       does, instead of leaving the entry to a %post scriptlet
#
# What the .spec layer checks (needs no rpm tooling, so it can run on any CI runner)
# ---------------------------------------------------------------------------------
#   S0  every `ln ... /usr/bin/<name>` in %post/%postun is guarded by an existence test
#       (`[ -e <src> ]` or `test -e <src>`) on that very source
#   S1  the same section also fails loudly (`exit <non-zero>`) when the source is absent
#   S2  a name %install stages as <buildroot>/usr/bin/<name> is also listed in %files
#   S3  no `ln ... /usr/bin/<name>` sits outside %post/%postun, where this script cannot see it
#   This is a static read of the spec text: it cannot prove what rpmbuild would actually do.
#
# The entries to check are taken from the *packaged* unit/desktop files and from the
# /usr/bin/<name> destinations the control scripts create, so the check follows the package
# rather than a hardcoded name. Only absolute /usr/bin/<name> references are asserted: a
# desktop entry that calls the program by bare name ("Exec=rustdesk %u") is resolved through
# PATH at run time and is deliberately not treated as an entry of this package.

set -uo pipefail

usage() {
    echo "usage: $0 <deb> [<deb> ...]" >&2
    echo "       $0 --rpm [--require-tools] <deb|rpm|spec> [...]" >&2
}

# ---- shared state ------------------------------------------------------------------------
# Globals on purpose: add_ref() appends to refs, and analyze_tree() resets all of them at
# entry, so there is no cross-input bleed. Nothing here relies on bash dynamic scoping.
fail_rc=0
skipped=0
refs=()
payloads=()
problems=()
pkg_scripts=()
literal_pairs=()   # "<src> <dst>"
loop_targets=()    # destinations of a `for x in a b; do ... ln ... /usr/bin/<dst>`
loop_names=()      # the names that loop iterates over

add_ref() {
    local p="${1%%[[:space:]]*}"
    case "$p" in
        /usr/bin/*) refs+=("${p##*/}") ;;
    esac
}

# Print the body of a spec section (e.g. `%post`, `%files`). The header may carry arguments
# (`%post -p /bin/bash`), and a section ends at the next lowercase section header.
spec_section() {
    awk -v want="$2" '
        $0 ~ "^%" want "([[:space:]].*)?$" { insec = 1; next }
        insec && $0 ~ "^%[a-z]" { insec = 0 }
        insec { print }
    ' "$1"
}

# Count `ln ... /usr/bin/...` lines on stdin. grep -c prints 0 and exits 1 when there is no
# match; set -e is off on purpose, so that is fine.
count_ln_usr_bin() {
    grep -E '(^|[^A-Za-z])ln[[:space:]]' | grep -c '/usr/bin/'
}

# Names a spec stages as <buildroot>/usr/bin/<name>. Trailing punctuation is stripped so a
# sentence in a %install comment ("... call, /usr/bin/rustdesk. NERV Desk") does not invent a
# package name that %files can never list.
staged_usr_bin_names() {
    grep -o '/usr/bin/[A-Za-z0-9._+-][A-Za-z0-9._+-]*' \
        | sed 's|.*/||; s/[.,;:)]*$//' \
        | sort -u
}

# Does the section contain an `exit <non-zero>` in statement position? Statements are split on
# `;`, `&&`, `||` and `then`, so `if [ ! -e ... ]; then exit 1; fi` counts exactly like a
# dedicated multi-line `exit 1`. The split is what keeps a mere mention inside a message
# (echo "could not exit 1") from being read as a guard.
has_fail_loud_exit() {
    awk '
        {
            line = $0
            gsub(/;/, ";\n", line)
            gsub(/&&/, "\n&&\n", line)
            gsub(/\|\|/, "\n||\n", line)
            gsub(/[[:space:]]then[[:space:]]/, "\nthen ", line)
            n = split(line, part, "\n")
            for (i = 1; i <= n; i++) {
                s = part[i]
                sub(/^[[:space:]]*(&&|\|\||then)[[:space:]]*/, "", s)
                if (s ~ /^[[:space:]]*exit[[:space:]]+[1-9]/) { found = 1 }
            }
        }
        END { print (found ? "yes" : "no") }
    '
}

# ---- the .spec layer (static, no rpm tools) ----------------------------------------------
check_spec() {
    local spec="$1"
    local section body dst src norm_src guard_ok exit_ok
    local total_ln checked_ln section_ln
    local inst files_body nm
    local gline ng

    if [ ! -f "$spec" ]; then
        echo "FAIL $spec: not a file"
        fail_rc=1
        return
    fi

    problems=()

    # S0/S1: an `ln` into /usr/bin/<name> must be guarded and must fail loudly.
    total_ln="$(count_ln_usr_bin < "$spec")"
    checked_ln=0
    for section in post postun; do
        body="$(spec_section "$spec" "$section")"
        [ -n "$body" ] || continue
        section_ln="$(printf '%s\n' "$body" | count_ln_usr_bin)"
        checked_ln=$(( checked_ln + section_ln ))
        [ "$section_ln" -gt 0 ] || continue

        while IFS= read -r gline; do
            [ -n "$gline" ] || continue
            dst="$(printf '%s\n' "$gline" | grep -o '/usr/bin/[A-Za-z0-9._+-]*' | head -n 1)"
            [ -n "$dst" ] || continue
            # source operand: the first word after `ln` that is not an option
            src=""
            seen_ln=0
            for w in $gline; do
                if [ "$seen_ln" -eq 0 ]; then
                    case "$w" in
                        ln|*/ln) seen_ln=1 ;;
                    esac
                    continue
                fi
                case "$w" in
                    -*) continue ;;
                esac
                src="$w"
                break
            done
            norm_src="${src//\"/}"
            norm_src="${norm_src//\'/}"
            if [ -z "$norm_src" ]; then
                problems+=("%$section: 'ln' to $dst has no readable source operand (cannot verify it)")
                continue
            fi
            # S0: an existence test on that same source, in the same section
            guard_ok=0
            while IFS= read -r gline2; do
                case "$gline2" in
                    *-e*) ;;
                    *) continue ;;
                esac
                ng="${gline2//\"/}"
                ng="${ng//\'/}"
                case "$ng" in
                    *"$norm_src"*) guard_ok=1; break ;;
                esac
            done <<< "$body"
            if [ "$guard_ok" -eq 0 ]; then
                problems+=("%$section: 'ln' to $dst from $norm_src is not guarded by an existence test on that source")
            fi
        done < <(printf '%s\n' "$body" | grep -E '(^|[^A-Za-z])ln[[:space:]]' | grep '/usr/bin/')

        # S1: fail loudly instead of leaving a dangling entry behind
        exit_ok="$(printf '%s\n' "$body" | has_fail_loud_exit)"
        if [ "$exit_ok" = no ]; then
            for dst in $(printf '%s\n' "$body" \
                            | grep -E '(^|[^A-Za-z])ln[[:space:]]' | grep '/usr/bin/' \
                            | grep -o '/usr/bin/[A-Za-z0-9._+-]*' | sort -u); do
                problems+=("%$section: 'ln' to $dst has no fail-loud 'exit <non-zero>' when the source is absent")
            done
        fi
    done

    # S3: an `ln` we did not look at
    if [ "$checked_ln" -ne "$total_ln" ]; then
        problems+=("a 'ln' into /usr/bin appears outside %post/%postun; this script only audits those sections")
    fi

    # S2: %install stages /usr/bin/<name> but %files never claims it
    inst="$(spec_section "$spec" install)"
    files_body="$(spec_section "$spec" files)"
    if [ -n "$inst" ]; then
        for nm in $(printf '%s\n' "$inst" | staged_usr_bin_names); do
            [ -n "$nm" ] || continue
            if ! printf '%s\n' "$files_body" | grep -qE "(^|[[:space:]])/usr/bin/$nm([[:space:]]|\$)"; then
                problems+=("%install stages /usr/bin/$nm but %files does not list it")
            fi
        done
    fi

    if [ "${#problems[@]}" -eq 0 ]; then
        echo "OK   $spec: spec-level guards present (no rpm tooling needed)"
    else
        echo "FAIL $spec"
        for p in "${problems[@]}"; do
            echo "     - $p"
        done
        fail_rc=1
    fi
}

# ---- unpack an rpm into the same shape a deb has ----------------------------------------
# Payload lands in $work/..., and the scriptlets are materialised as $work/DEBIAN/postinst so
# the R1/R2 resolution rules work unchanged.
unpack_rpm() {
    local rpm="$1" work="$2"
    ( cd "$work" && rpm2cpio "$rpm" | cpio -idm --quiet ) || return 1
    mkdir -p "$work/DEBIAN" || return 1
    rpm -qp --scripts "$rpm" > "$work/DEBIAN/postinst" || return 1
    return 0
}

# ---- resolve the entries of one unpacked tree -------------------------------------------
# $1 label (printed), $2 work dir, $3 1 = the package may ship /usr/bin/<name> itself (rpm)
analyze_tree() {
    local label="$1" work="$2" allow_bin="$3"
    local share="$work/usr/share/rustdesk"
    local f d s line name pair src dst t n u dup ok

    payloads=()
    problems=()
    refs=()
    pkg_scripts=()
    literal_pairs=()
    loop_targets=()
    loop_names=()

    # ---- the payload ----------------------------------------------------------------
    # A regular file directly in /usr/share/rustdesk that is not a shared object: the
    # bundled .so files are libraries, not the program the entries run.
    if [ -d "$share" ]; then
        while IFS= read -r f; do
            [ -n "$f" ] || continue
            case "${f##*/}" in
                *.so|*.so.*) continue ;;
            esac
            payloads+=("${f##*/}")
        done < <(find "$share" -maxdepth 1 -type f 2>/dev/null | sort)
    fi
    # A package with no /usr/share/rustdesk payload is only acceptable for an rpm, where
    # %install may place the binary straight at /usr/bin/<name> (res/rpm.spec:38).
    if [ "${#payloads[@]}" -eq 0 ] && [ "$allow_bin" -eq 0 ]; then
        problems+=("no payload executable in /usr/share/rustdesk (only libraries there?)")
    fi

    # ---- required entries -----------------------------------------------------------
    # unit files: ExecStart=<path> [args]. /usr/lib/systemd/system is where an rpm keeps
    # them; the deb keeps the same file under /usr/share/rustdesk/files and copies it there
    # in postinst, so listing both paths is harmless for a deb.
    while IFS= read -r line; do
        case "$line" in
            ExecStart=*) add_ref "${line#ExecStart=}" ;;
        esac
    done < <(grep -rhs '^ExecStart=' \
                "$work/etc" "$work/usr/lib/systemd/system" "$share/files" 2>/dev/null; true)
    # desktop files: Exec=<path> [args] (%U etc. is stripped by add_ref)
    while IFS= read -r line; do
        case "$line" in
            Exec=*) add_ref "${line#Exec=}" ;;
        esac
    done < <(grep -rhs '^Exec=' \
                "$work/usr/share/applications" "$share/files" 2>/dev/null; true)

    # control scripts: destinations of literal `ln [-f] [-s] <src> /usr/bin/<name>`
    for d in "$work/DEBIAN"; do
        [ -d "$d" ] || continue
        while IFS= read -r f; do
            [ -n "$f" ] || continue
            pkg_scripts+=("$f")
        done < <(find "$d" -maxdepth 1 -type f 2>/dev/null | sort)
    done

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
        # R0: the payload is shipped under the very name the entry uses. `-e` follows
        # symlinks, so a dangling symlink does NOT count as a payload: it would install
        # cleanly and leave /usr/bin/<name> pointing at nothing.
        if [ ! -d "$share/$name" ] && [ -e "$share/$name" ]; then ok=1; fi
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
        # R3: the package ships the entry itself (res/rpm.spec installs /usr/bin/rustdesk)
        if [ "$ok" -eq 0 ] && [ "$allow_bin" -eq 1 ]; then
            if [ ! -d "$work/usr/bin/$name" ] && [ -e "$work/usr/bin/$name" ]; then ok=1; fi
        fi
        if [ "$ok" -eq 0 ]; then
            problems+=("entry '$name' does not resolve: no /usr/share/rustdesk/$name payload, no /usr/bin/$name in the package, and no control script links it from an existing payload")
        fi
    done

    if [ "${#problems[@]}" -eq 0 ]; then
        echo "OK   $label: payload [${payloads[*]:-}] provides [${uniq_refs[*]:-}]"
    else
        echo "FAIL $label"
        for p in "${problems[@]}"; do
            echo "     - $p"
        done
        fail_rc=1
    fi
}

# ---- main --------------------------------------------------------------------------------
rpm_mode=0
require_tools=0
inputs=()

for arg in "$@"; do
    case "$arg" in
        --rpm) rpm_mode=1 ;;
        --require-tools) require_tools=1 ;;
        --*) echo "unknown option: $arg" >&2; usage; exit 2 ;;
        *) inputs+=("$arg") ;;
    esac
done

if [ "${#inputs[@]}" -lt 1 ]; then
    usage
    exit 2
fi

kinds=()
for f in "${inputs[@]}"; do
    case "$f" in
        *.spec)
            if [ "$rpm_mode" -ne 1 ]; then
                echo "$f: a .spec input needs --rpm" >&2
                exit 2
            fi
            kinds+=("spec") ;;
        *.rpm)
            if [ "$rpm_mode" -ne 1 ]; then
                echo "$f: a .rpm input needs --rpm" >&2
                exit 2
            fi
            kinds+=("rpm") ;;
        *) kinds+=("deb") ;;
    esac
done

# The tool set follows the layer that will actually run: the .spec layer needs neither dpkg-deb
# nor rpm tooling, so it is usable on a plain CI runner; the .rpm artifact layer needs both
# rpm2cpio and cpio, which no CI container here installs yet.
need_deb=0
need_rpm=0
need_spec=0
skip_rpm=0
i=0
for f in "${inputs[@]}"; do
    case "${kinds[$i]}" in
        deb) need_deb=1 ;;
        rpm) need_rpm=1 ;;
        spec) need_spec=1 ;;
    esac
    i=$((i + 1))
done

if [ "$need_rpm" -eq 1 ]; then
    missing_rpm=""
    for tool in rpm2cpio cpio rpm; do
        command -v "$tool" >/dev/null 2>&1 || missing_rpm="$missing_rpm $tool"
    done
    if [ -n "$missing_rpm" ]; then
        if [ "$require_tools" -eq 1 ]; then
            echo "missing required tool(s) for the .rpm artifact layer:$missing_rpm" >&2
            exit 2
        fi
        skip_rpm=1
        echo "SKIP .rpm artifact layer: missing tool(s):$missing_rpm (pass --require-tools to fail instead)"
        echo "::warning::.rpm artifact layer skipped, missing tool(s):$missing_rpm"
    fi
fi

base_tools="mktemp"
[ "$need_deb" -eq 1 ] && base_tools="$base_tools dpkg-deb find sort grep sed"
[ "$need_rpm" -eq 1 ] && [ "$skip_rpm" -eq 0 ] && base_tools="$base_tools find sort grep sed"
[ "$need_spec" -eq 1 ] && base_tools="$base_tools awk grep sed"
for tool in $base_tools; do
    command -v "$tool" >/dev/null 2>&1 || { echo "missing required tool: $tool" >&2; exit 2; }
done

i=0
for f in "${inputs[@]}"; do
    kind="${kinds[$i]}"
    i=$((i + 1))

    case "$kind" in
        spec)
            check_spec "$f"
            ;;
        deb)
            if [ ! -f "$f" ]; then
                echo "FAIL $f: not a file"
                fail_rc=1
                continue
            fi
            work="$(mktemp -d)" || { echo "cannot create a temp dir" >&2; exit 2; }
            if ! dpkg-deb -R "$f" "$work" 2>/dev/null; then
                echo "FAIL $f: dpkg-deb -R failed (not a deb?)"
                rm -rf "$work"
                fail_rc=1
                continue
            fi
            analyze_tree "$f" "$work" 0
            rm -rf "$work"
            ;;
        rpm)
            if [ "$skip_rpm" -eq 1 ]; then
                skipped=1
                echo "SKIP $f: .rpm artifact layer not checked (missing tool(s):$missing_rpm)"
                continue
            fi
            if [ ! -f "$f" ]; then
                echo "FAIL $f: not a file"
                fail_rc=1
                continue
            fi
            work="$(mktemp -d)" || { echo "cannot create a temp dir" >&2; exit 2; }
            if ! unpack_rpm "$f" "$work"; then
                echo "FAIL $f: could not unpack the rpm (rpm2cpio/cpio/rpm failed?)"
                rm -rf "$work"
                fail_rc=1
                continue
            fi
            analyze_tree "$f" "$work" 1
            rm -rf "$work"
            ;;
    esac
done

if [ "$fail_rc" -ne 0 ]; then
    exit 1
fi
if [ "$skipped" -ne 0 ]; then
    exit 3
fi
exit 0
