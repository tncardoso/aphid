# Build vhs gifs for the landing page
gifs: gif-aphid gif-alate gif-colony

gif-aphid: (_gif "aphid")

gif-alate: (_gif "alate")

gif-colony: (_gif "colony")

_gif name:
    cd site/assets && vhs {{name}}.tape

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
