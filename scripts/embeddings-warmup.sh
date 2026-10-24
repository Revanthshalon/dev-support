#!/bin/sh
# embeddings-warmup: run by the `embeddings-warmup` compose service (profile `embeddings`).
# Waits for Memgraph's Bolt port, then loads EMBEDDINGS_MODEL through the MAGE `embeddings`
# module, so a bad model name or provider key fails now instead of at the first `ops index`.
# Exits non-zero on failure, with key-like tokens masked in the error.
set -e

: "${MEMGRAPH_HOST:=memgraph}"
: "${EMBEDDINGS_MODEL:=all-MiniLM-L6-v2}"

mg() {
  mgconsole --host "$MEMGRAPH_HOST" --port 7687 --username "$MEMGRAPH_USER" --password "$MEMGRAPH_PASSWORD"
}

i=0
until echo 'RETURN 1;' | mg >/dev/null 2>&1; do
  i=$((i + 1))
  [ "$i" -ge 60 ] && { echo "memgraph not reachable on bolt after 120s" >&2; exit 1; }
  sleep 2
done

echo "loading embeddings model $EMBEDDINGS_MODEL"
q="CALL embeddings.model_info({model_name: '$EMBEDDINGS_MODEL'}) YIELD info RETURN info;"
# Provider errors can echo part of a key: mask sk-/hf_/Bearer tokens and key=/key: values.
if out=$(echo "$q" | mg 2>&1); then
  echo "$out"
else
  echo "$out" | sed -E 's#(sk-|hf_|bearer +|key["'\'']?[=:] *["'\'']?)[A-Za-z0-9_+/=-]{6,}#\1***#Ig' >&2
  exit 1
fi
