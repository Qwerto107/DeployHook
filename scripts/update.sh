#!/usr/bin/env bash
set -Eeuo pipefail
umask 077
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
export DOCKER_HOST=unix:///run/docker.sock
unset DOCKER_CONTEXT COMPOSE_FILE COMPOSE_PROJECT_NAME COMPOSE_PROFILES

log() {
    printf '%s %s\n' "$(date --iso-8601=seconds)" "$*"
}

finish() {
    local code=$?
    if (( code == 0 )); then
        log "更新成功"
    else
        log "更新失败，退出码：$code；请检查服务实际状态"
    fi
}

# 使用受保护的固定目录保存锁，手动更新也必须经过此脚本。
exec 9>/data/deployhook/state/update.lock
if ! flock -n 9; then
    log "已有更新正在执行，跳过本次请求"
    exit 75
fi
trap finish EXIT

# 按实际业务修改目录与项目名；更新脚本不要求放在 DeployHook 目录中。
cd /data/myapp
compose=(/usr/bin/docker compose --project-name myapp --file /data/myapp/compose.yaml)
log "开始更新"
"${compose[@]}" config --quiet
"${compose[@]}" pull
# 先拉取再替换，不主动执行 down；所有长期运行的服务应配置 healthcheck。
"${compose[@]}" up --detach --no-build --pull never --wait --wait-timeout 120
