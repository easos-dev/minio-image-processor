#!/bin/sh
# 在受信目标服务器显式运行一次，不开启 trace，不将 mc 原始输出送入日志。
# 根凭据仅用于初始化；EasOS 只拿专用桶用户，图片处理器只接收短期 GET URL。
set -eu
umask 077
root_user=$(cat /setup/minio-secrets/minio_root_user)
root_password=$(cat /setup/minio-secrets/minio_root_password)
app_user=$(cat /setup/secrets/access_key)
app_password=$(cat /setup/secrets/secret_key)
# MC_HOST_* 通过环境读取，避免 alias 命令把根密码加入参数或配置日志。
# 原部署脚本的根凭据是字母/数字/十六进制；拒绝 URL 特殊字符。
case "$root_user$root_password" in *[!a-zA-Z0-9_-]*) echo '根凭据含 URL 特殊字符，需改用安全编码的客户端配置' >&2; exit 1;; esac
export MC_HOST_storage="http://${root_user}:${root_password}@minio:9000"
step() {
    label=$1; shift
    if "$@" >/dev/null 2>&1; then printf '%s\n' "$label: 完成"; else printf '%s\n' "$label: 失败（未输出可能含凭据的上游诊断）" >&2; exit 1; fi
}
# 不清空、不接管已有桶；新桶名由 prepare.py 校验。
# stat 已存在时直接终止，避免把其他实例的桶当作新环境使用。
if mc stat "storage/$BUCKET" >/dev/null 2>&1; then
    echo '桶已存在，初始化停止；请核对归属后使用已有配置，勿重复创建用户或替换权限' >&2
    exit 1
fi
step '创建专用桶' mc mb --region us-east-1 "storage/$BUCKET"
# 固定社区版本的桶级 CORS 为空实现，由 HTTPS 网关控制浏览器来源。
step '创建应用用户' mc admin user add storage "$app_user" "$app_password"
step '创建应用策略' mc admin policy create storage "${BUCKET}-application" /setup/generated/policy.json
step '绑定应用策略' mc admin policy attach storage "${BUCKET}-application" --user "$app_user"
printf '%s\n' '初始化完成；凭据只保存在服务器 secrets，EasOS 配置见 generated/easos-bucket.example.toml'
