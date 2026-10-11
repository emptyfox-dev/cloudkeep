#!/bin/sh
#
# 把 dist/ 下的二进制发布为 CNB Release 资产。
#
# 为什么需要这个脚本：构建产物只存在于流水线工作区，构建结束即消失，
# 用户拿不到可下载的二进制。这里通过 CNB Release API 把它们挂到版本上。
#
# 用法：
#   TOKEN=<令牌> ./scripts/publish-release.sh dist
#
# 令牌权限：需要 `repo-release:rw`（只读令牌会报
#   errcode 10023 / Missing required scopes: repo-release:rw）
#
# 环境变量：
#   TOKEN          必填，CNB 访问令牌
#   CNB_REPO       仓库路径，默认 Emptyfox-Network/CloudKeep
#   API_BASE       默认 https://api.cnb.cool
#   TAG            版本标签，默认 v<Cargo.toml 里的 version>
#   TARGET_SHA     标签指向的提交，默认当前 HEAD
#   STRICT=1       发布失败时以非 0 退出（默认只告警，不影响镜像构建）
set -eu

DIST_DIR="${1:-dist}"
API_BASE="${API_BASE:-https://api.cnb.cool}"
REPO="${CNB_REPO:-Emptyfox-Network/CloudKeep}"

: "${TOKEN:=}"
if [ -z "$TOKEN" ]; then
    echo "[release] 未提供 TOKEN，跳过发布"
    exit 0
fi

if [ ! -d "$DIST_DIR" ]; then
    echo "[release] 目录不存在: $DIST_DIR，跳过发布"
    exit 0
fi

VERSION="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml | head -n1)"
TAG="${TAG:-v${VERSION:-0.0.0}}"
TARGET_SHA="${TARGET_SHA:-$(git rev-parse HEAD 2>/dev/null || echo '')}"

# JSON 取值：优先 python3，其次 jq，最后退回 sed（只适用于扁平 JSON）
# 注意先确认 python3 真的可用：Windows 上的应用商店别名会存在但不可执行
if command -v python3 >/dev/null 2>&1 && python3 -c 'pass' >/dev/null 2>&1; then
    JSON_TOOL=python3
elif command -v jq >/dev/null 2>&1; then
    JSON_TOOL=jq
else
    JSON_TOOL=sed
fi
echo "[release] JSON 解析方式: $JSON_TOOL"
if [ "$JSON_TOOL" = "sed" ]; then
    echo "[release] 警告：未找到 python3/jq，改用 sed 做尽力解析；"
    echo "[release]       服务端返回字段顺序变化时可能取错值，建议安装 jq 或 python3"
fi

json_get() {
    key="$1"
    case "$JSON_TOOL" in
        python3)
            python3 -c 'import json,sys;d=json.load(sys.stdin);print(d.get(sys.argv[1]) or "")' "$key" 2>/dev/null
            ;;
        jq)
            jq -r --arg k "$key" '.[$k] // empty' 2>/dev/null
            ;;
        *)
            sed -n "s/.*\"$key\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" | head -n1
            ;;
    esac
}

echo "[release] 仓库=$REPO 标签=$TAG 提交=${TARGET_SHA:-未指定}"

# ---------- 1. 创建（或复用）release ----------
BODY=$(printf '单文件静态二进制（Rust），无需运行时依赖。\n\n- 授权：cloudkeep auth-url / auth-code <code>\n- 配置项见 .env.example\n')
PAYLOAD=$(printf '{"tag_name":"%s","target_commitish":"%s","name":"CloudKeep %s","body":"%s","draft":false,"prerelease":false,"make_latest":"true"}' \
    "$TAG" "$TARGET_SHA" "$TAG" "$BODY")

RESP_FILE="$(mktemp)"
CODE=$(curl -sS -o "$RESP_FILE" -w '%{http_code}' \
    -X POST "$API_BASE/$REPO/-/releases" \
    -H "Authorization: Bearer $TOKEN" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    -d "$PAYLOAD") || CODE=000

RELEASE_ID="$(json_get id < "$RESP_FILE" || true)"

if [ "$CODE" -ge 300 ] || [ -z "$RELEASE_ID" ]; then
    echo "[release] 创建 release 返回 $CODE: $(cat "$RESP_FILE" | head -c 300)"

    # 已存在则按 tag 取回
    CODE2=$(curl -sS -o "$RESP_FILE" -w '%{http_code}' \
        -X GET "$API_BASE/$REPO/-/releases/tags/$TAG" \
        -H "Authorization: Bearer $TOKEN" -H 'Accept: application/json') || CODE2=000
    RELEASE_ID="$(json_get id < "$RESP_FILE" || true)"

    if [ -z "$RELEASE_ID" ]; then
        echo "[release] 无法创建或找到 release（HTTP $CODE/$CODE2）"
        echo "[release] 提示：令牌需要 repo-release:rw 权限"
        [ "${STRICT:-0}" = "1" ] && exit 1
        rm -f "$RESP_FILE"
        exit 0
    fi
    echo "[release] 复用已存在的 release: $RELEASE_ID"
else
    echo "[release] 已创建 release: $RELEASE_ID"
fi

# ---------- 2. 逐个上传资产 ----------
FAILED=0
for FILE in "$DIST_DIR"/*; do
    [ -f "$FILE" ] || continue
    NAME="$(basename "$FILE")"
    SIZE="$(wc -c < "$FILE" | tr -d ' ')"

    echo "[release] 上传 $NAME（$SIZE 字节）"

    UP_PAYLOAD=$(printf '{"asset_name":"%s","size":%s,"overwrite":true}' "$NAME" "$SIZE")
    CODE=$(curl -sS -o "$RESP_FILE" -w '%{http_code}' \
        -X POST "$API_BASE/$REPO/-/releases/$RELEASE_ID/asset-upload-url" \
        -H "Authorization: Bearer $TOKEN" \
        -H 'Content-Type: application/json' \
        -H 'Accept: application/json' \
        -d "$UP_PAYLOAD") || CODE=000

    UPLOAD_URL="$(json_get upload_url < "$RESP_FILE" || true)"
    VERIFY_URL="$(json_get verify_url < "$RESP_FILE" || true)"

    if [ -z "$UPLOAD_URL" ]; then
        echo "[release]   取上传地址失败（HTTP $CODE）: $(head -c 200 "$RESP_FILE")"
        FAILED=1
        continue
    fi

    CODE=$(curl -sS -o /dev/null -w '%{http_code}' \
        -X PUT "$UPLOAD_URL" \
        -H 'Content-Type: application/octet-stream' \
        --data-binary "@$FILE") || CODE=000

    if [ "$CODE" -ge 300 ]; then
        echo "[release]   上传失败（HTTP $CODE）"
        FAILED=1
        continue
    fi

    if [ -n "$VERIFY_URL" ]; then
        CODE=$(curl -sS -o "$RESP_FILE" -w '%{http_code}' \
            -X POST "$VERIFY_URL" \
            -H "Authorization: Bearer $TOKEN" \
            -H 'Accept: application/json') || CODE=000
        [ "$CODE" -ge 300 ] && echo "[release]   确认上传返回 HTTP $CODE"
    fi

    echo "[release]   ✓ $NAME"
done

rm -f "$RESP_FILE"

if [ "$FAILED" = "1" ]; then
    echo "[release] 部分资产上传失败"
    [ "${STRICT:-0}" = "1" ] && exit 1
    exit 0
fi

echo "[release] 完成：$API_BASE/../$REPO 的 $TAG 版本已更新"
