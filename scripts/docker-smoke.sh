#!/bin/sh
# Start a localmail image the way the README shows and check that SMTP and an API send arrive.
# Usage: scripts/docker-smoke.sh [image]   (default: localmail:smoke)
set -eu
image=${1:-localmail:smoke}
port=${PORT:-9432}
smtp_port=${SMTP_PORT:-2532}
name=localmail-smoke-$$
trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT

docker run -d --name "$name" -p "$port:9402" -p "$smtp_port:2525" -e "LOCALMAIL_BASE_URL=http://localhost:$port" \
  "$image" >/dev/null

fail() { echo "FAIL: $1"; docker logs "$name" 2>&1 | tail -20; exit 1; }
for _ in $(seq 1 50); do
  curl -sf "http://localhost:$port/health" >/dev/null 2>&1 && break
  sleep 0.2
done
curl -sf "http://localhost:$port/health" >/dev/null || fail "/health didn't answer"

curl -sf "http://localhost:$port/emails" -H 'authorization: Bearer re_smoke' -H 'content-type: application/json' \
  -d '{"from":"smoke@localmail.test","to":"api@example.com","subject":"API smoke","text":"ok"}' >/dev/null \
  || fail "the Resend API didn't accept a send"

# A minimal SMTP conversation; nc keeps the connection open long enough to read the replies.
printf 'EHLO smoke\r\nMAIL FROM:<smoke@localmail.test>\r\nRCPT TO:<smtp@example.com>\r\nDATA\r\nSubject: SMTP smoke\r\n\r\nok\r\n.\r\nQUIT\r\n' \
  | nc -w 3 localhost "$smtp_port" | grep -q "^250 2.0.0 OK: queued" || fail "SMTP didn't accept a message"

curl -sf "http://localhost:$port/dev/api/messages/wait?to=smtp@example.com&timeout=5s" | grep -q '"SMTP smoke"' \
  || fail "the SMTP message isn't in the inbox"
curl -sf "http://localhost:$port/dev/api/messages?to=api@example.com" | grep -q '"API smoke"' \
  || fail "the API message isn't in the inbox"
echo "ok: $image answers over HTTP and SMTP"
