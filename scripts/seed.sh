#!/usr/bin/env bash
# 等待 SQL Server 就绪并灌入示例数据
set -euo pipefail

CONTAINER="${CONTAINER:-mssql-transfer-test}"
SA_PASSWORD="${SA_PASSWORD:-YourStrong!Passw0rd}"
SQLCMD="/opt/mssql-tools18/bin/sqlcmd"

echo "等待 SQL Server 启动…"
for i in $(seq 1 40); do
  if docker exec "$CONTAINER" "$SQLCMD" -S localhost -U sa -P "$SA_PASSWORD" -C -Q "SELECT 1" >/dev/null 2>&1; then
    echo "已就绪（${i} 次探测）"
    break
  fi
  if [ "$i" -eq 40 ]; then
    echo "SQL Server 启动超时，请检查 docker compose logs" >&2
    exit 1
  fi
  sleep 3
done

echo "导入示例库…"
docker cp "$(dirname "$0")/init.sql" "$CONTAINER:/tmp/init.sql"
docker exec "$CONTAINER" "$SQLCMD" -S localhost -U sa -P "$SA_PASSWORD" -C -i /tmp/init.sql

echo "完成。连接参数：localhost:1433  sa / $SA_PASSWORD  数据库 TransferDemo"
