#!/bin/sh

# Viber Router — Claude Code Setup Script (macOS/Linux)
# Auto-generated — configures Claude Code to use your API key

set -e

ENDPOINT_URL="{{ENDPOINT_URL}}"
API_KEY='{{API_KEY}}'
HAIKU_MODEL="{{HAIKU}}"
OPUS_MODEL="{{OPUS}}"
SONNET_MODEL="{{SONNET}}"
SUBAGENT_MODEL="{{SUBAGENT}}"
TRACKING_URL=""

RED=$(printf '\033[0;31m')
GREEN=$(printf '\033[0;32m')
YELLOW=$(printf '\033[1;33m')
BLUE=$(printf '\033[0;34m')
NC=$(printf '\033[0m')

echo "${BLUE}================================${NC}"
echo "${BLUE}  Viber Router Claude Code Setup${NC}"
echo "${BLUE}================================${NC}"
echo ""

if [ -z "$ENDPOINT_URL" ]; then
    echo "${RED}Error: Endpoint URL not configured${NC}"
    exit 1
fi
if [ -z "$API_KEY" ]; then
    echo "${RED}Error: API key not configured${NC}"
    exit 1
fi

MASKED_KEY=$(echo "$API_KEY" | cut -c 1-10)
echo "Endpoint URL: ${GREEN}$ENDPOINT_URL${NC}"
echo "API Key:      ${GREEN}${MASKED_KEY}...${NC}"
echo ""

# Returns non-zero when the file exists but could not be copied (read-only dir,
# disk full, ...). Callers must never modify a file after a failed backup.
backup_file() {
    f_path="$1"
    if [ -f "$f_path" ]; then
        if ! cp "$f_path" "${f_path}.backup.$(date +%Y%m%d%H%M%S)"; then
            echo "${RED}  Error: could not back up $f_path${NC}"
            return 1
        fi
        echo "${YELLOW}  Backed up: $f_path${NC}"
    fi
    return 0
}

# Claude Code reads these from the process environment and they take precedence
# over ~/.claude/settings.json. Stale values from older installs would silently
# override the configuration written below, so they must be removed.
LEGACY_VAR_ALTERNATION='ANTHROPIC_BASE_URL|ANTHROPIC_AUTH_TOKEN|ANTHROPIC_API_KEY|ANTHROPIC_DEFAULT_HAIKU_MODEL|ANTHROPIC_DEFAULT_SONNET_MODEL|ANTHROPIC_DEFAULT_OPUS_MODEL|CLAUDE_CODE_SUBAGENT_MODEL'

# Matches assignments with or without a leading "export" and with or without
# leading whitespace, plus the comment markers written by older versions.
LEGACY_LINE_PATTERN="^[[:space:]]*(export[[:space:]]+)?($LEGACY_VAR_ALTERNATION)=|^[[:space:]]*#[[:space:]]*(Viber Router|Claude Code) configuration[[:space:]]*\$"

# Returns 0 only when the file existed, contained legacy lines, and was cleaned.
# The file is backed up only when it actually needs changing.
remove_claude_vars() {
    f_path="$1"
    if [ ! -f "$f_path" ]; then
        return 1
    fi
    if ! grep -qE "$LEGACY_LINE_PATTERN" "$f_path" 2>/dev/null; then
        return 1
    fi
    # Never rewrite an rc file that we failed to back up.
    if ! backup_file "$f_path"; then
        echo "${YELLOW}  Skipped $f_path - refusing to modify it without a backup${NC}"
        echo "${YELLOW}  Remove the legacy lines manually, or fix permissions and re-run${NC}"
        return 1
    fi
    f_tmp="${f_path}.viberrouter.tmp"
    if sed -E "/$LEGACY_LINE_PATTERN/d" "$f_path" > "$f_tmp" 2>/dev/null && cp "$f_tmp" "$f_path"; then
        rm -f "$f_tmp"
        return 0
    fi
    rm -f "$f_tmp"
    echo "${YELLOW}  Warning: could not clean $f_path${NC}"
    return 1
}

# Install statusline script
install_statusline() {
    local statusline_file="$HOME/.claude/statusline.sh"
    mkdir -p "$HOME/.claude"

    echo "${BLUE}Installing statusline script...${NC}"

    # Write statusline script inline (no external download needed)
    cat > "$statusline_file" << 'STATUSLINE_EOF'
#!/bin/bash
# Viber Router Statusline Script for Claude Code
# Hiển thị balance, token usage và cost trên statusline

TRACKING_URL="PLACEHOLDER_TRACKING_URL"
API_KEY='PLACEHOLDER_API_KEY'

CACHE_DIR="${TMPDIR:-/tmp}/viberrouter-statusline"
CACHE_FILE="$CACHE_DIR/tracking_cache.json"
CACHE_TTL=30

mkdir -p "$CACHE_DIR" 2>/dev/null

INPUT=""
if [ ! -t 0 ]; then
    INPUT=$(cat)
fi

parse_context() {
    local json="$1"
    CWD=$(echo "$json" | grep -o '"cwd"[[:space:]]*:[[:space:]]*"[^"]*"' | sed 's/.*:.*"\([^"]*\)"/\1/' | head -1)
    MODEL=$(echo "$json" | grep -o '"display_name"[[:space:]]*:[[:space:]]*"[^"]*"' | sed 's/.*:.*"\([^"]*\)"/\1/' | head -1)
    MODEL_ID=$(echo "$json" | grep -o '"model"[[:space:]]*:[[:space:]]*{[^}]*"id"[[:space:]]*:[[:space:]]*"[^"]*"' | grep -o '"id"[[:space:]]*:[[:space:]]*"[^"]*"' | sed 's/.*:.*"\([^"]*\)"/\1/' | head -1)
    if [ -z "$MODEL_ID" ]; then
        MODEL_ID=$(echo "$json" | grep -o '"id"[[:space:]]*:[[:space:]]*"[^"]*"' | sed 's/.*:.*"\([^"]*\)"/\1/' | head -1)
    fi
    INPUT_TOKENS=$(echo "$json" | grep -o '"input_tokens"[[:space:]]*:[[:space:]]*[0-9]*' | sed 's/.*:[[:space:]]*//' | head -1)
    CACHE_CREATION=$(echo "$json" | grep -o '"cache_creation_input_tokens"[[:space:]]*:[[:space:]]*[0-9]*' | sed 's/.*:[[:space:]]*//' | head -1)
    CACHE_READ=$(echo "$json" | grep -o '"cache_read_input_tokens"[[:space:]]*:[[:space:]]*[0-9]*' | sed 's/.*:[[:space:]]*//' | head -1)
    OUTPUT_TOKENS=$(echo "$json" | grep -o '"output_tokens"[[:space:]]*:[[:space:]]*[0-9]*' | sed 's/.*:[[:space:]]*//' | head -1)
    INPUT_TOKENS="${INPUT_TOKENS:-0}"
    CACHE_CREATION="${CACHE_CREATION:-0}"
    CACHE_READ="${CACHE_READ:-0}"
    OUTPUT_TOKENS="${OUTPUT_TOKENS:-0}"
    CONVERSATION_TOKENS=$((INPUT_TOKENS + CACHE_CREATION + CACHE_READ))
    MAX_TOKENS=$(echo "$json" | grep -o '"context_window_size"[[:space:]]*:[[:space:]]*[0-9]*' | sed 's/.*:[[:space:]]*//' | head -1)
    LINES_ADDED=$(echo "$json" | grep -o '"total_lines_added"[[:space:]]*:[[:space:]]*[0-9]*' | sed 's/.*:[[:space:]]*//' | head -1)
    LINES_REMOVED=$(echo "$json" | grep -o '"total_lines_removed"[[:space:]]*:[[:space:]]*[0-9]*' | sed 's/.*:[[:space:]]*//' | head -1)
    GIT_NUM_FILES=$(echo "$json" | grep -o '"gitNumStagedOrUnstagedFilesChanged"[[:space:]]*:[[:space:]]*[0-9]*' | sed 's/.*:[[:space:]]*//' | head -1)
    USED_PERCENTAGE=$(echo "$json" | grep -o '"used_percentage"[[:space:]]*:[[:space:]]*[0-9]*' | sed 's/.*:[[:space:]]*//' | head -1)
    CWD="${CWD:-$(pwd)}"
    GIT_NUM_FILES="${GIT_NUM_FILES:-0}"
    MODEL="${MODEL:-unknown}"
    MODEL_ID="${MODEL_ID:-}"
    CONVERSATION_TOKENS="${CONVERSATION_TOKENS:-0}"
    MAX_TOKENS="${MAX_TOKENS:-200000}"
    LINES_ADDED="${LINES_ADDED:-0}"
    LINES_REMOVED="${LINES_REMOVED:-0}"
    USED_PERCENTAGE="${USED_PERCENTAGE:-0}"
    GIT_BRANCH=""
    if [ -n "$CWD" ] && [ -d "$CWD" ] && command -v git >/dev/null 2>&1; then
        if git -C "$CWD" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
            GIT_BRANCH=$(git -C "$CWD" branch --show-current 2>/dev/null)
        fi
    fi
}

shorten_path() {
    local path="$1"
    local home="$HOME"
    path="${path/#$home/\~}"
    if [ ${#path} -gt 40 ]; then
        path=$(echo "$path" | awk -F'/' '{print $(NF-1)"/"$NF}')
    fi
    echo "$path"
}

format_model() {
    local model="$1"
    if echo "$model" | grep -qiE '(opus|sonnet|haiku)'; then
        local tier version
        tier=$(echo "$model" | grep -oiE '(opus|sonnet|haiku)' | head -1)
        tier="$(echo "${tier:0:1}" | tr '[:lower:]' '[:upper:]')${tier:1}"
        version=$(echo "$model" | grep -oE '[0-9]+[\.\-][0-9]+' | tail -1 | tr '-' '.')
        if [ -n "$version" ]; then
            echo "${tier}-${version}"
        else
            echo "$tier"
        fi
        return
    fi
    echo "$model" | cut -c1-18
}

format_tokens() {
    local num="$1"
    if [ "$num" -ge 1000000 ] 2>/dev/null; then
        echo "$((num / 1000000)).$(( (num % 1000000) / 100000 ))M"
    elif [ "$num" -ge 1000 ] 2>/dev/null; then
        echo "$((num / 1000))k"
    else
        echo "$num"
    fi
}

format_vnd() {
    local num="$1"
    local int_val
    int_val=$(printf "%.0f" "$num" 2>/dev/null || echo "$num")
    echo "$int_val" | sed ':a;s/\B[0-9]\{3\}\>/.&/;ta'
}

get_tracking() {
    local now
    now=$(date +%s)
    if [ -f "$CACHE_FILE" ]; then
        local cache_time
        cache_time=$(stat -c %Y "$CACHE_FILE" 2>/dev/null || stat -f %m "$CACHE_FILE" 2>/dev/null || echo "0")
        local age=$((now - cache_time))
        if [ "$age" -lt "$CACHE_TTL" ]; then
            cat "$CACHE_FILE"
            return 0
        fi
    fi
    if [ -n "$TRACKING_URL" ] && [ -n "$API_KEY" ]; then
        local response
        response=$(curl -s --max-time 5 -H "Authorization: Bearer $API_KEY" "$TRACKING_URL" 2>/dev/null)
        if [ -n "$response" ]; then
            echo "$response" > "$CACHE_FILE"
            echo "$response"
            return 0
        fi
    fi
    if [ -f "$CACHE_FILE" ]; then
        cat "$CACHE_FILE"
    fi
}

parse_tracking() {
    local json="$1"
    BALANCE=$(echo "$json" | grep -o '"balance"[[:space:]]*:[[:space:]]*[0-9.]*' | sed 's/.*:[[:space:]]*//' | head -1)
    LAST_TOKENS=$(echo "$json" | grep -o '"total_tokens"[[:space:]]*:[[:space:]]*[0-9]*' | sed 's/.*:[[:space:]]*//' | tail -1)
    LAST_COST=$(echo "$json" | grep -o '"total_cost"[[:space:]]*:[[:space:]]*[0-9.]*' | sed 's/.*:[[:space:]]*//' | tail -1)
    BALANCE="${BALANCE:-0}"
    LAST_TOKENS="${LAST_TOKENS:-0}"
    LAST_COST="${LAST_COST:-0}"
}

progress_bar() {
    local current="$1"
    local max="$2"
    local width=5
    if [ "$max" -eq 0 ] 2>/dev/null; then
        echo "▯▯▯▯▯"
        return
    fi
    local pct=$((current * 100 / max))
    local filled=$((pct * width / 100))
    local empty=$((width - filled))
    local bar=""
    for ((i=0; i<filled; i++)); do bar+="\033[32m▮\033[0m"; done
    for ((i=0; i<empty; i++)); do bar+="\033[90m▯\033[0m"; done
    echo "$bar"
}

main() {
    parse_context "$INPUT"
    local W="\033[97m" G="\033[32m" Y="\033[33m" R="\033[31m" C="\033[36m" DM="\033[90m" D="\033[0m"

    local line1=""
    local short_cwd
    short_cwd=$(shorten_path "$CWD")
    line1+="${C}${short_cwd}${D}"
    if [ -n "$GIT_BRANCH" ]; then
        line1+="  ${DM}⎇${D} ${W}${GIT_BRANCH}${D}"
        if [ "$GIT_NUM_FILES" -gt 0 ]; then
            line1+=" ${Y}(${GIT_NUM_FILES})${D}"
        fi
    fi
    if [ "$LINES_ADDED" -gt 0 ] || [ "$LINES_REMOVED" -gt 0 ]; then
        line1+="  ${G}+${LINES_ADDED}${D} ${R}-${LINES_REMOVED}${D}"
    fi

    local line2=""
    # Use used_percentage from Claude Code if available, otherwise calculate
    local ctx_pct=0
    if [ "$USED_PERCENTAGE" -gt 0 ] 2>/dev/null; then
        ctx_pct=$USED_PERCENTAGE
    elif [ "$MAX_TOKENS" -gt 0 ]; then
        ctx_pct=$((CONVERSATION_TOKENS * 100 / MAX_TOKENS))
    fi
    local ctx_bar
    ctx_bar=$(progress_bar "$ctx_pct" "100")
    line2+="$ctx_bar ${W}${ctx_pct}%${D}"

    local model_display
    model_display=$(format_model "$MODEL")
    line2+=" ${DM}│${D} ${W}${model_display}${D}"

    local tracking_data
    tracking_data=$(get_tracking)
    if [ -n "$tracking_data" ]; then
        parse_tracking "$tracking_data"
        if [ -n "$BALANCE" ] && [ "$BALANCE" != "0" ]; then
            local bal_fmt
            bal_fmt=$(format_vnd "$BALANCE")
            line2+=" ${DM}│${D} ${G}${bal_fmt}đ${D}"
        fi
        if [ -n "$LAST_TOKENS" ] && [ "$LAST_TOKENS" != "0" ]; then
            local last_tok_fmt last_cost_fmt
            last_tok_fmt=$(format_tokens "$LAST_TOKENS")
            last_cost_fmt=$(format_vnd "$LAST_COST")
            line2+=" ${DM}│${D} ${DM}Cost:${D} ${W}${last_tok_fmt} tokens${D} ${Y}${last_cost_fmt}đ${D}"
        fi
    fi

    echo -e "$line1"
    echo -e "$line2"
}

main
STATUSLINE_EOF

    # Replace placeholders with actual values
    sed -i "s|PLACEHOLDER_TRACKING_URL|$TRACKING_URL|g" "$statusline_file" 2>/dev/null || \
        sed -i '' "s|PLACEHOLDER_TRACKING_URL|$TRACKING_URL|g" "$statusline_file"
    sed -i "s|PLACEHOLDER_API_KEY|$API_KEY|g" "$statusline_file" 2>/dev/null || \
        sed -i '' "s|PLACEHOLDER_API_KEY|$API_KEY|g" "$statusline_file"

    chmod +x "$statusline_file"
    echo "  ${GREEN}✓ Installed ~/.claude/statusline.sh${NC}"
    return 0
}

update_settings_json() {
    settings_file="$HOME/.claude/settings.json"
    statusline_installed="$1"
    statusline_cmd="$HOME/.claude/statusline.sh"

    mkdir -p "$HOME/.claude"

    if ! command -v jq >/dev/null 2>&1; then
        echo ""
        echo "${RED}Error: jq is required but not installed.${NC}"
        echo ""
        echo "Please install jq first:"
        echo "  ${BLUE}macOS:${NC}        brew install jq"
        echo "  ${BLUE}Ubuntu/Debian:${NC} sudo apt-get install -y jq"
        echo "  ${BLUE}Fedora/RHEL:${NC}   sudo dnf install -y jq"
        echo "  ${BLUE}Arch Linux:${NC}    sudo pacman -S jq"
        echo ""
        echo "Then run this script again."
        exit 1
    fi

    if [ ! -f "$settings_file" ]; then
        echo '{}' > "$settings_file"
    else
        backup_file "$settings_file"
    fi

    tmp_file=$(mktemp)

    if [ "$statusline_installed" = "true" ]; then
        jq --arg url "$ENDPOINT_URL" --arg key "$API_KEY" \
           --arg haiku "$HAIKU_MODEL" --arg opus "$OPUS_MODEL" --arg sonnet "$SONNET_MODEL" \
           --arg subagent "$SUBAGENT_MODEL" \
           --arg sl_cmd "$statusline_cmd" '
            .env.ANTHROPIC_BASE_URL = $url |
            .env.ANTHROPIC_AUTH_TOKEN = $key |
            .env.ANTHROPIC_DEFAULT_HAIKU_MODEL = $haiku |
            .env.ANTHROPIC_DEFAULT_OPUS_MODEL = $opus |
            .env.ANTHROPIC_DEFAULT_SONNET_MODEL = $sonnet |
            .env.CLAUDE_CODE_SUBAGENT_MODEL = $subagent |
            .disableLoginPrompt = true |
            .includeCoAuthoredBy = false |
            .statusLine = {"type": "command", "command": $sl_cmd}
        ' "$settings_file" > "$tmp_file" && mv "$tmp_file" "$settings_file"
    else
        jq --arg url "$ENDPOINT_URL" --arg key "$API_KEY" \
           --arg haiku "$HAIKU_MODEL" --arg opus "$OPUS_MODEL" --arg sonnet "$SONNET_MODEL" \
           --arg subagent "$SUBAGENT_MODEL" '
            .env.ANTHROPIC_BASE_URL = $url |
            .env.ANTHROPIC_AUTH_TOKEN = $key |
            .env.ANTHROPIC_DEFAULT_HAIKU_MODEL = $haiku |
            .env.ANTHROPIC_DEFAULT_OPUS_MODEL = $opus |
            .env.ANTHROPIC_DEFAULT_SONNET_MODEL = $sonnet |
            .env.CLAUDE_CODE_SUBAGENT_MODEL = $subagent |
            .disableLoginPrompt = true |
            .includeCoAuthoredBy = false
        ' "$settings_file" > "$tmp_file" && mv "$tmp_file" "$settings_file"
    fi
}

# Collects legacy variables that are still set in this shell.
# Callers pass "${VAR+x}" so that a variable set to an empty string still counts
# as present - an empty value also overrides settings.json.
# ACTIVE_VARS is intentionally space-prefixed so it can be appended to "unset".
ACTIVE_VARS=""
note_active_var() {
    if [ -n "$2" ]; then
        ACTIVE_VARS="$ACTIVE_VARS $1"
    fi
}

echo "${BLUE}Cleaning up legacy shell environment variables...${NC}"

CLEANED_ANY=0
for rc_file in \
    "$HOME/.bashrc" \
    "$HOME/.zshrc" \
    "$HOME/.bash_profile" \
    "$HOME/.zprofile" \
    "$HOME/.profile" \
    "$HOME/.zshenv"
do
    if remove_claude_vars "$rc_file"; then
        echo "  ${GREEN}✓ Cleaned $rc_file${NC}"
        CLEANED_ANY=1
    fi
done

if [ "$CLEANED_ANY" -eq 0 ]; then
    echo "  No legacy variables found in shell startup files"
fi

note_active_var ANTHROPIC_BASE_URL "${ANTHROPIC_BASE_URL+x}"
note_active_var ANTHROPIC_AUTH_TOKEN "${ANTHROPIC_AUTH_TOKEN+x}"
note_active_var ANTHROPIC_API_KEY "${ANTHROPIC_API_KEY+x}"
note_active_var ANTHROPIC_DEFAULT_HAIKU_MODEL "${ANTHROPIC_DEFAULT_HAIKU_MODEL+x}"
note_active_var ANTHROPIC_DEFAULT_SONNET_MODEL "${ANTHROPIC_DEFAULT_SONNET_MODEL+x}"
note_active_var ANTHROPIC_DEFAULT_OPUS_MODEL "${ANTHROPIC_DEFAULT_OPUS_MODEL+x}"
note_active_var CLAUDE_CODE_SUBAGENT_MODEL "${CLAUDE_CODE_SUBAGENT_MODEL+x}"

if [ -n "$ACTIVE_VARS" ]; then
    echo ""
    echo "${YELLOW}  Warning: these variables are still set in your current shell:${NC}"
    for var_name in $ACTIVE_VARS; do
        echo "${YELLOW}    - $var_name${NC}"
    done
    echo "${YELLOW}  They override ~/.claude/settings.json for any Claude Code started here.${NC}"
    echo "${YELLOW}  This script runs in a subshell, so it cannot unset them for you.${NC}"
    echo "${YELLOW}  Open a new terminal, or run: unset$ACTIVE_VARS${NC}"
fi

echo ""
STATUSLINE_INSTALLED="false"
if [ -n "$TRACKING_URL" ]; then
    if install_statusline; then
        STATUSLINE_INSTALLED="true"
    fi
else
    echo "${YELLOW}  Statusline: Skipped (no tracking URL)${NC}"
fi

echo ""
echo "${BLUE}Configuring Claude Code settings...${NC}"
update_settings_json "$STATUSLINE_INSTALLED"
echo "  ${GREEN}✓ Updated ~/.claude/settings.json${NC}"

echo ""
echo "${GREEN}================================${NC}"
echo "${GREEN}  Configuration Complete!${NC}"
echo "${GREEN}================================${NC}"
echo ""
echo "Claude Code is now configured:"
echo "  Endpoint:   ${BLUE}$ENDPOINT_URL${NC}"
echo "  API Key:    ${BLUE}${MASKED_KEY}...${NC}"
echo "  Config:     ${BLUE}~/.claude/settings.json${NC}"
if [ "$STATUSLINE_INSTALLED" = "true" ]; then
    echo "  Statusline: ${GREEN}Enabled${NC} (balance, tokens, cost)"
fi
echo ""
echo "All settings live in settings.json - no environment variables are set."
echo ""
echo "${YELLOW}Next steps:${NC}"
echo "  1. Open a new terminal (existing ones may still hold the removed variables)"
echo "  2. Run: ${BLUE}claude${NC}"
echo ""
