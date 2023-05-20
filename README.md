# OxHost

A small static file hosting utility with extreme performance utilizing modern optimizations.

# Features

- In-memory file caching
- Automatic HTML, JS and CSS file minimization(in-memory)
- Caching of minimized files
- Live, low-latency updates from the filesystem

# Roadmap

- [ ] A better caching solution<br />
      To allow limiting total cache memory usage
- [ ] Config file
- [ ] CLI interface
- [ ] Client capability-specific meta-files<br />
      Route plaintext targets to a `page-plaintext` file if one exists, route HTML targets to `page` and embed targets to `page-embed` if it exists
