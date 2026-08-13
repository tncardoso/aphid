gifs:
    cd site/assets && vhs aphid.tape

# Build the full site: the book first, then Hugo.
#
# The order matters. mdbook renders into site/static/docs/, and Hugo copies
# that tree into site/public/docs/ as it builds. Run Hugo first and the docs
# are missing or stale.
site *ARGS:
    mdbook build
    # --cleanDestinationDir because mdbook fingerprints its stylesheets
    # (aphid-<hash>.css). Without it, every edit leaves the previous hash
    # behind in site/public/ to be deployed as dead weight.
    cd site && hugo --minify --cleanDestinationDir {{ARGS}}

# The same site, served on this machine.
serve:
    mdbook build
    cd site && hugo server
