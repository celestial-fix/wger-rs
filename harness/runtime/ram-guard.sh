#!/bin/sh
# Abort if available RAM under 1GB (kB).
avail=$(awk '/MemAvailable:/ {print $2}' /proc/meminfo)
echo "MemAvailable_kB=$avail"
if [ "$avail" -lt 1048576 ]; then
  echo "ABORT: available RAM under 1GB" >&2
  exit 9
fi
