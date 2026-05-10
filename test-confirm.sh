#!/bin/sh
# Simple test script that asks for confirmation
echo "This script will greet you."
echo "Do you want to proceed? (yes/no)"
read answer
if [ "$answer" = "yes" ]; then
  echo "Hello from $(hostname)!"
  echo "Current time: $(date)"
  echo "Uptime: $(uptime)"
  echo "Done."
else
  echo "Aborted by user."
fi
