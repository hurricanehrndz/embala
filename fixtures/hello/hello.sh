#!/bin/sh
# macOS executable stand-in for the .app/.pkg fixtures: prints the marker and
# touches a probe file so remote verification can check it ran.
echo "hello from embala fixture 0.1.0"
touch /tmp/embala-hello-ran
