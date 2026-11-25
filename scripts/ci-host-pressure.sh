#!/usr/bin/env bash
# One timestamped line of runner-side scheduling telemetry, for the CI VM
# jobs that serve a guest (ci-qemu-benchmark.sh serve): host load, CPU
# pressure (PSI), and the runner cgroup's CPU pressure and throttle counters.
# A counter this kernel or container does not expose is printed as `missing`,
# never guessed. HOST_PRESSURE_PROC and HOST_PRESSURE_CGROUP name other roots,
# for the tests.
set -uo pipefail
proc="${HOST_PRESSURE_PROC:-/proc}"
cgroup="${HOST_PRESSURE_CGROUP:-/sys/fs/cgroup}"

# `some avg10=1.00 avg60=...` -> `some.avg10=1.00 some.avg60=...`
pressure() {
    local prefix="$1" file="$2" kind fields field out=''
    if [ ! -r "$file" ]; then printf '%s=missing' "$prefix"; return; fi
    while read -r kind fields; do
        [ -n "$kind" ] || continue
        for field in $fields; do out+="$prefix.$kind.$field "; done
    done <"$file"
    [ -n "$out" ] && printf '%s' "${out% }" || printf '%s=missing' "$prefix"
}

# The named keys of a cgroup cpu.stat, each `missing` when absent.
cpu_stat() {
    local prefix="$1" file="$2" key value
    shift 2
    for key in "$@"; do
        value=''
        [ ! -r "$file" ] || value="$(awk -v k="$key" '$1 == k { print $2; exit }' "$file")"
        printf ' %s.%s=%s' "$prefix" "$key" "${value:-missing}"
    done
}

line="host-pressure $(date -u +%FT%TZ)"
if [ -r "$proc/loadavg" ]; then
    read -r one five fifteen tasks _ <"$proc/loadavg"
    line+=" loadavg=$one/$five/$fifteen tasks=$tasks"
else
    line+=' loadavg=missing'
fi
line+=" nproc=$(nproc 2>/dev/null || echo missing)"
line+=" $(pressure psi.cpu "$proc/pressure/cpu")"
if [ -r "$cgroup/cpu.stat" ] && [ -e "$cgroup/cgroup.controllers" ]; then
    # cgroup v2
    line+=" $(pressure cgroup.cpu "$cgroup/cpu.pressure")"
    line+="$(cpu_stat cgroup "$cgroup/cpu.stat" nr_periods nr_throttled throttled_usec)"
    if [ -r "$cgroup/cpu.max" ]; then
        line+=" cgroup.cpu.max=$(tr ' ' '/' <"$cgroup/cpu.max")"
    else
        line+=' cgroup.cpu.max=missing'
    fi
elif [ -r "$cgroup/cpu/cpu.stat" ] || [ -r "$cgroup/cpu,cpuacct/cpu.stat" ]; then
    # cgroup v1
    stat="$cgroup/cpu/cpu.stat"
    [ -r "$stat" ] || stat="$cgroup/cpu,cpuacct/cpu.stat"
    line+=" cgroup.cpu.pressure=missing"
    line+="$(cpu_stat cgroup "$stat" nr_periods nr_throttled throttled_time)"
else
    line+=' cgroup.cpu.pressure=missing cgroup.cpu.stat=missing'
fi
printf '%s\n' "$line"
