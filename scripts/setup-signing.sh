#!/usr/bin/env bash
# Prints a code-signing identity (a certificate's SHA-1) that stays the same across rebuilds.
#
# macOS ties Screen Recording, Accessibility and Microphone grants to a signature's designated
# requirement. An ad-hoc signature's is its cdhash, which every rebuild changes, so the grants
# would go each time. A Mac without a Developer ID gets the next best thing: a self-signed
# certificate, whose requirement names the certificate and not the build.
#
# It lives in a keychain of its own, unlocked by a generated password in a file beside it, so
# nothing asks: no GUI prompt, no sudo, and the login keychain is left alone. The certificate isn't
# trusted (trusting one needs an admin prompt) and doesn't need to be: codesign and TCC only compare
# it. Run it as often as you like; install-app.sh does, as a reboot locks the keychain.
set -euo pipefail

NAME="Scrawl Local Signing"
KEYCHAIN="${HOME}/Library/Keychains/scrawl-signing.keychain-db"
PASSWORD_FILE="${HOME}/Library/Keychains/scrawl-signing.password"
# Replacing the certificate drops the grants, so make it outlast the Mac.
DAYS=7300

identity() {
  security find-identity -p codesigning "${KEYCHAIN}" |
    awk '!id && $1 ~ /^[0-9]+\)$/ { id = $2 } END { if (id) print id }'
}

umask 077

if [[ ! -f "${KEYCHAIN}" ]]; then
  /usr/bin/openssl rand -hex 32 >"${PASSWORD_FILE}"
  security create-keychain -p "$(<"${PASSWORD_FILE}")" "${KEYCHAIN}"
fi
if [[ ! -f "${PASSWORD_FILE}" ]]; then
  echo "setup-signing: ${KEYCHAIN} has no ${PASSWORD_FILE}. Delete the keychain to start again; macOS will ask for its grants once more." >&2
  exit 1
fi
PASSWORD="$(<"${PASSWORD_FILE}")"

security unlock-keychain -p "${PASSWORD}" "${KEYCHAIN}"
# No settings at all: no timeout, and no locking on sleep.
security set-keychain-settings "${KEYCHAIN}"

if [[ -z "$(identity)" ]]; then
  WORK="$(mktemp -d)"
  trap 'rm -rf "${WORK}"' EXIT
  cat >"${WORK}/cert.cnf" <<EOF
[req]
distinguished_name = name
x509_extensions = code_signing
prompt = no
[name]
CN = ${NAME}
[code_signing]
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
EOF
  # The system's LibreSSL on purpose: `security import` can't read Homebrew OpenSSL 3's PKCS#12.
  /usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days "${DAYS}" \
    -config "${WORK}/cert.cnf" -keyout "${WORK}/key.pem" -out "${WORK}/cert.pem" 2>/dev/null
  /usr/bin/openssl pkcs12 -export -inkey "${WORK}/key.pem" -in "${WORK}/cert.pem" \
    -name "${NAME}" -passout "pass:${PASSWORD}" -out "${WORK}/identity.p12"
  security import "${WORK}/identity.p12" -k "${KEYCHAIN}" -P "${PASSWORD}" -T /usr/bin/codesign >/dev/null
fi

# Without this, codesign stops to ask for the password in a window.
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "${PASSWORD}" "${KEYCHAIN}" >/dev/null

# `list-keychains -s` replaces the list, so what's there goes back with ours after it.
KEYCHAINS=()
while IFS= read -r line; do
  line="${line#"${line%%[![:space:]]*}"}"
  line="${line#\"}"
  KEYCHAINS+=("${line%\"}")
done < <(security list-keychains -d user)
case " ${KEYCHAINS[*]} " in
  *" ${KEYCHAIN} "*) ;;
  *) security list-keychains -d user -s "${KEYCHAINS[@]}" "${KEYCHAIN}" ;;
esac

ID="$(identity)"
if [[ -z "${ID}" ]]; then
  echo "setup-signing: no identity in ${KEYCHAIN} after setting it up" >&2
  exit 1
fi
echo "${ID}"
