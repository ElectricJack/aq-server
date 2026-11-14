#!/usr/bin/env bash
# Inventory check of the playbook-authoring plan (Task F1): every piece of the
# old compile path is either removed or has a caller on a tool or save path.
#
#   scripts/check-authoring-inventory.sh ENGINE_DIR
#
# ENGINE_DIR is a checkout of aq-playbooks. The engine rules are also applied
# to the copy vendored here (vendor/aq-playbooks-*). Prints one line per
# finding and exits 1 if there is any. There is no allowlist: an open finding
# keeps this script failing until the code is gone.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENGINE="${1:-}"
if [ -z "$ENGINE" ] || [ ! -d "$ENGINE/crates" ]; then
    echo "usage: $0 ENGINE_DIR (a checkout of aq-playbooks)" >&2
    exit 2
fi
ENGINE="$(cd "$ENGINE" && pwd)"
SELF="scripts/$(basename "$0")"
findings=0

finding() {
    findings=$((findings + 1))
    echo "FINDING: $1"
    [ -n "${2:-}" ] && printf '%s\n' "$2" | sed 's/^/    /'
}

# search REGEX DIR...: matching lines in source files, build output left out.
search() {
    local regex="$1"
    shift
    grep -rnHE --exclude="$(basename "$SELF")" --exclude-dir=target --exclude-dir=node_modules --exclude-dir=__pycache__ \
        -e "$regex" "$@" 2>/dev/null
}

# gone LABEL REGEX DIR...: a finding for every match.
gone() {
    local label="$1" regex="$2" hits
    shift 2
    hits="$(search "$regex" "$@")"
    [ -n "$hits" ] && finding "$label" "$hits"
}

# called LABEL REGEX FILE: a finding unless FILE has a line matching REGEX.
called() {
    if [ ! -f "$3" ]; then
        finding "$1: $3 does not exist"
    elif ! grep -qE -e "$2" "$3"; then
        finding "$1: no match for /$2/ in $3"
    fi
}

# body FILE FN: the text of top-level function FN in FILE.
body() {
    awk -v start="^(pub )?fn $2[(<]" '$0 ~ start {on = 1} on {print} on && /^}/ {exit}' "$1"
}

# called_in LABEL REGEX FILE FN: a finding unless FN's body in FILE matches.
called_in() {
    if [ ! -f "$3" ]; then
        finding "$1: $3 does not exist"
    elif ! body "$3" "$4" | grep -qE -e "$2"; then
        finding "$1: no match for /$2/ in fn $4 of $3"
    fi
}

cd "$ROOT" || exit 2
SERVER_DIRS=(crates scripts protocol)

# ---------------------------------------------------------------- REMOVE
# The server's old routing compiler.
gone "aq_routing::{policy,plan,compile,fixtures} is still named" \
    'aq_routing::(policy|plan|compile|fixtures)\b' "${SERVER_DIRS[@]}"
gone "aq-routing still declares an old compiler module" \
    '^[[:space:]]*(pub )?mod (policy|plan|compile|fixtures)\b' crates/aq-routing/src
for module in policy plan compile fixtures; do
    for path in "crates/aq-routing/src/$module.rs" "crates/aq-routing/src/$module"; do
        [ -e "$path" ] && finding "old compiler module still exists: $path"
    done
done
gone "COMPILER_INSTRUCTIONS remains" 'COMPILER_INSTRUCTIONS' "${SERVER_DIRS[@]}"
# routing_policies may be named only by the migration that made the table
# (0120), the one that drops it (0121) and the test of that drop.
hits="$(search 'routing_policies' "${SERVER_DIRS[@]}" |
    grep -vE '^crates/aq-server/migrations/012[01][^/]*:' |
    grep -vE '^crates/aq-server/tests/it_schema/routing_policies_migration\.rs:' |
    grep -vE '^crates/aq-server/tests/it_schema/main\.rs:[0-9]+:mod routing_policies_migration;')"
[ -n "$hits" ] && finding "routing_policies is named outside migrations 0120/0121 and their test" "$hits"
gone "struct Classification remains in aq-routing" 'struct Classification\b' crates/aq-routing

# The engine's old compile surface: in the checkout and in the vendored copy.
ENGINES=("$ENGINE")
for vendored in "$ROOT"/vendor/aq-playbooks-*/; do
    [ -d "${vendored}crates" ] && ENGINES+=("${vendored%/}")
done
for engine in "${ENGINES[@]}"; do
    gone "unsupported_features or Severity::Question remains ($engine)" \
        'unsupported_features|Severity::Question' "$engine/crates"
    gone "Command::{Surface,Interfaces,Simulate} remains ($engine)" \
        'Command::(Surface|Interfaces|Simulate)\b' "$engine/crates"
    gone "fake_host or pilots is still a product module of the engine CLI ($engine)" \
        'pub mod (fake_host|pilots)\b' "$engine/crates/aq-playbooks-cli/src"
done
gone "unsupported_features or Severity::Question remains (server)" \
    'unsupported_features|Severity::Question' "${SERVER_DIRS[@]}"

# The routing pilot, in whichever pilots.rs it lives (the server's moved from
# aq-server/src/playbooks/coding/ to aq-playbook-domain/src/coding/; the
# engine's moved from the CLI's src/ to its tests/support/).
while IFS= read -r pilots; do
    gone "the routing pilot remains" 'pub fn routing\(\)' "$pilots"
done < <(
    for dir in "$ROOT/crates" "${ENGINES[@]/%//crates}"; do
        find "$dir" -name target -prune -o -name pilots.rs -print
    done
)

# ------------------------------------------------------------- REPURPOSE
CLI="$ENGINE/crates/aq-playbooks-cli/src/authoring.rs"
COMPILER="$ENGINE/crates/aq-playbooks-compiler/src"
AUTHORING="crates/aq-playbook-authoring/src"

called "draft::seal has no caller in the agent tool" 'draft::seal\(' "$CLI"
called "draft::seal has no caller on the save path" 'draft::seal\(' "$AUTHORING/save.rs"
called "run_cases has no caller in the agent tool" '\brun_cases\(' "$CLI"
called "run_cases has no caller on the save path" '\brun_cases\(' "$AUTHORING/save.rs"
called_in "semantic_diff is not what compare uses" '\bsemantic_diff\(' "$COMPILER/draft.rs" compare
called_in "parse_source_header is not reached from validate" '\bparse_source_header\(' "$COMPILER/lib.rs" validate
called "the draft path does not go through validate" '\bvalidate\(' "$COMPILER/draft.rs"
# "Contracts equal the pinned catalog" (was artifacts::validate) is checked at
# a save by check_saved, which the completion of an authoring run calls.
called_in "the save path does not hold contracts to the pinned catalog" \
    'catalog\.(commands|events)\.get\(' "$AUTHORING/save.rs" check_saved
called "check_saved has no caller on the save path" 'save::check_saved\(' "$AUTHORING/complete.rs"
called "retain_registry is not used for the coding artifacts" '\bretain_registry\(' \
    crates/aq-playbook-domain/src/coding.rs
called "retain_registry is not used for the authoring workspace" '\bretain_registry\(' "$AUTHORING/request.rs"
called "route_needed_emissions is not used by routing" 'route_needed_emissions' \
    crates/aq-server/src/routing.rs
called "route_needed_emissions is not read by the event collector" 'route_needed_emissions' \
    crates/aq-server/src/playbooks/triggers.rs

if [ "$findings" -gt 0 ]; then
    echo "check-authoring-inventory: $findings finding(s)"
    exit 1
fi
echo "check-authoring-inventory: no findings (${#ENGINES[@]} engine tree(s) checked)"
