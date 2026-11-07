#!/usr/bin/env bash
set -euo pipefail

: "${PDFCRAFT_ORACLE_DIR:?set PDFCRAFT_ORACLE_DIR through GITHUB_ENV}"
base="$PDFCRAFT_ORACLE_DIR/corpus"
mkdir -p "$base/pdfcraft-fixtures" "$base/pdfcraft-public-domain" "$base/pdfcraft-real" "$base/all"

fetch() {
  local url="$1" path="$2" sha="$3"
  curl --fail --location --retry 3 --silent --show-error --user-agent 'Mozilla/5.0 (compatible; PdfCraftCorpus/1.0)' "$url" -o "$path"
  printf '%s  %s\n' "$sha" "$path" | sha256sum --check --status || {
    echo "PDF corpus checksum mismatch: $path" >&2
    exit 1
  }
}

fetch 'https://www.irs.gov/pub/irs-pdf/f1040.pdf' "$base/irs-f1040-2025.pdf" '3d31c226df0d189ced80e039d01cf0f8820c1019681a0f0ca6264de277b7e982'
fetch 'https://www.irs.gov/pub/irs-pdf/f990.pdf' "$base/irs-f990-2025.pdf" '0b715d2790691ca6d4fb16ba0923e3cdf1d0e04cd26ea48788d41a708e945533'
fetch 'https://www.opm.gov/Forms/pdf_fill/sf85p.pdf' "$base/pdfcraft-public-domain/opm-sf85p-2017.pdf" 'd2f5168d71aba229f2b98b9b33c729833331156ce384e0010435b647608bf2e1'
fetch 'https://www.govinfo.gov/content/pkg/USCODE-2023-title26/pdf/USCODE-2023-title26.pdf' "$base/uscode-title26-2023.pdf" '0b8dd9e1d8132c09a2d56c60ec827a1195b61ce53a4a285a15255de29ad656ba'
cp crates/ui-egui/tests/data/form.pdf "$base/pdfcraft-fixtures/form.pdf"
printf '%s  %s\n' 'c6f8b656e225101129aa36bd5467adc8dba16730915f19caca17f6a546d0b040' "$base/pdfcraft-fixtures/form.pdf" | sha256sum --check --status || {
  echo 'PDF fixture checksum mismatch' >&2
  exit 1
}

for pdf in "$base"/*.pdf "$base"/pdfcraft-fixtures/*.pdf "$base"/pdfcraft-public-domain/*.pdf "$base"/pdfcraft-real/*.pdf; do
  [ -f "$pdf" ] || continue
  ln -sf "$pdf" "$base/all/$(basename "$pdf")"
done
count=$(find "$base/all" -maxdepth 1 -type l -name '*.pdf' | wc -l)
[ "$count" -ge 5 ] || { echo "expected at least five oracle PDFs; found $count" >&2; exit 1; }
echo "Staged $count pinned PDF inputs in $base/all"
