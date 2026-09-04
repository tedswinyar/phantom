# Hosting this site

**This directory is a stub on purpose. There is no deployable code here.**

The site is plain static output — `hugo build` writes `public/`, and any static
host can serve it. Deployment is the one part of the website layer a template
genuinely cannot ship for you, because it needs an account, a domain, and a
DNS zone that only you have.

## What to do at stamp time

Pick one of these and delete the rest of this file:

**Any static host.** Point Netlify, Cloudflare Pages, GitHub Pages, S3 with a
website endpoint, or a plain web server at `public/`. For most projects this is
the right answer: it is a static marketing site, and none of the below buys you
anything a CDN-backed bucket does not.

**S3 + CloudFront behind your own domain, provisioned as code.** This is the
pattern the template was extracted from, and it is worth the extra pieces if
you already own a domain and want the whole thing reproducible:

- An S3 bucket holding the Hugo output, not public — reachable only through the
  distribution, via an origin access control
- A CloudFront distribution in front of it, with a default root object and a
  404 mapping so clean URLs work
- An ACM certificate in `us-east-1` (CloudFront only reads certificates from
  that region, which is the detail that costs everyone an afternoon the first
  time) plus the DNS records that validate it
- A DNS alias record pointing your hostname at the distribution
- A deploy step that syncs `public/` to the bucket and then **invalidates the
  distribution** — CloudFront will keep serving the old HTML until you do

Write it as an infrastructure-as-code app (CDK, Terraform, Pulumi — whichever
you already know) in this directory, and keep the account ID, the profile name,
the domain, and the hosted zone ID in a gitignored config file rather than in
the source. Ship the shape, not the credentials.

## Two things to get right whichever route you take

**Cache invalidation.** The theme fingerprints its stylesheet, so the CSS URL
changes whenever the CSS does and a stale cache can never pair new HTML with
old CSS. HTML has no such protection: if your host caches it, the deploy has to
invalidate it explicitly.

**Nothing secret in the repo.** Account identifiers, bucket names, and
distribution IDs are not credentials, but publishing them tells a stranger
exactly what to probe. Put them in a gitignored config and read them at deploy
time — the same convention `scripts/release.conf.example` uses for the Apple
signing identity.
