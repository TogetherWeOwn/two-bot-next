#!/bin/sh
# Remove only the stock non-Essential mount package; let apt preserve dependencies.
set -eu
export LC_ALL=C

format='${binary:Package}\t${Version}\t${Architecture}\t${Essential}\t${Status}\n'
identity=$(dpkg-query -W -f="$format" mount)
expected=$(printf 'mount\t2.38.1-5+deb12u3\tamd64\tno\tinstall ok installed\n')
if [ "$identity" != "$expected" ]; then
    printf 'Expected the exact stock non-Essential mount package; refusing purge\n' >&2
    exit 1
fi

apt-get -o APT::Get::AutomaticRemove=false check
before=$(dpkg-query -W -f="$format")
tab=$(printf '\t')
mount_records=$(printf '%s\n' "$before" | while IFS= read -r line; do
    if [ "${line%%"$tab"*}" = mount ]; then
        printf '%s\n' "$line"
    fi
done)
expected_after=$(printf '%s\n' "$before" | while IFS= read -r line; do
    if [ "${line%%"$tab"*}" != mount ]; then
        printf '%s\n' "$line"
    fi
done)
if [ "$mount_records" != "$identity" ] || [ -z "$expected_after" ]; then
    printf 'Incomplete or inconsistent package inventory; refusing purge\n' >&2
    exit 1
fi
plan=$(apt-get -o APT::Get::AutomaticRemove=false --simulate purge mount)
actions=$(printf '%s\n' "$plan" | while IFS=' ' read -r operation package rest; do
    case "$operation" in
        Purg|Remv|Inst|Conf) printf '%s %s\n' "$operation" "$package" ;;
    esac
done)
if [ "$actions" != 'Purg mount' ]; then
    printf 'Apt plan is not exactly a mount-only purge; refusing package changes\n' >&2
    printf '%s\n' "$plan" >&2
    exit 1
fi

# Check every dpkg record, not just affected packages. No other version, status,
# Essential flag or architecture may change as a side effect of this operation.
apt-get -o APT::Get::AutomaticRemove=false --yes purge mount
after=$(dpkg-query -W -f="$format")
if [ "$after" != "$expected_after" ]; then
    printf 'Installed package inventory changed beyond mount; failing image build\n' >&2
    exit 1
fi
apt-get -o APT::Get::AutomaticRemove=false check
