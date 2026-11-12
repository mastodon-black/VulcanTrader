/* VulcanTrader theme switch - light is the default skin, dark is opt-in.

   Load this synchronously in <head>, before the stylesheet paints: it stamps
   data-theme on <html> immediately so a saved dark choice never flashes white.
   The palettes themselves live in theme.css (:root = light,
   :root[data-theme="dark"] = dark); this file only picks one.

   Default is light regardless of the OS colour-scheme preference - the
   dashboards are meant to open white, and only the toggle changes that.

   Public surface (window.VulcanTheme):
     get()            -> 'light' | 'dark'
     set(theme)       -> switch and remember
     toggle()
     css('--pos')     -> the resolved CSS variable, for canvas/SVG libraries that
                         cannot take var(...) (Plotly, lightweight-charts)
     rgba('--pos', a) -> that variable's colour at alpha a (uses the --*-rgb triplets)
     chart()          -> every colour the chart libraries need, resolved for the
                         active skin (axis text, grids, candle up/down, series palette)
   A 'vulcan-theme' event fires on window after every change (also when another
   tab switches), so pages can redraw their charts. Any element with a
   data-theme-toggle attribute toggles on click - works inside Alpine/Vue
   templates without wiring a handler into either. */
(function () {
    'use strict';

    var KEY = 'VulcanTrader_theme';
    var root = document.documentElement;

    function stored() {
        try {
            var v = localStorage.getItem(KEY);
            return v === 'dark' || v === 'light' ? v : null;
        } catch (e) { return null; }   // private window / blocked storage
    }

    function get() {
        return root.getAttribute('data-theme') === 'dark' ? 'dark' : 'light';
    }

    function apply(theme, persist) {
        theme = theme === 'dark' ? 'dark' : 'light';
        var changed = theme !== get();
        root.setAttribute('data-theme', theme);
        if (changed) cache = Object.create(null);
        if (persist) {
            try { localStorage.setItem(KEY, theme); } catch (e) { /* not fatal */ }
        }
        if (changed) {
            window.dispatchEvent(new CustomEvent('vulcan-theme', { detail: { theme: theme } }));
        }
    }

    // getComputedStyle().getPropertyValue forces a synchronous style recalculation when the
    // DOM is dirty, and chart code asks for ~30 colours per redraw (the dashboards redraw on
    // every poll that brings new data). A variable's value only changes with the theme, so
    // cache per theme and drop the cache when it flips.
    var cache = Object.create(null);

    function css(name) {
        var v = cache[name];
        if (v === undefined) {
            v = getComputedStyle(root).getPropertyValue(name).trim();
            if (v) cache[name] = v;   // never cache "not loaded yet"
        }
        return v;
    }

    function rgba(name, alpha) {
        return 'rgba(' + css(name + '-rgb') + ',' + alpha + ')';
    }

    function chart() {
        var series = [];
        for (var i = 1; i <= 6; i++) series.push(css('--chart-series-' + i));
        return {
            bg: css('--chart-bg'),
            text: css('--chart-text'),
            textDim: css('--text-dim'),
            fg: css('--text'),
            title: css('--chart-title'),
            grid: css('--chart-grid'),
            grid3d: css('--chart-grid-3d'),
            zero: css('--chart-zero'),
            guide: css('--chart-guide'),
            guideSoft: css('--chart-guide-soft'),
            border: css('--chart-border'),
            entryLine: css('--chart-entry-line'),
            legendBg: css('--chart-legend-bg'),
            starEdge: css('--chart-star-edge'),
            faint: css('--text-faint'),
            soft: css('--text-soft'),
            pos: css('--pos'),
            neg: css('--neg'),
            neu: css('--neu'),
            accent: css('--accent'),
            accentTxt: css('--accent-txt'),
            series: series
        };
    }

    // First paint: no event (nothing is listening yet, and nothing has changed).
    root.setAttribute('data-theme', stored() || 'light');

    window.VulcanTheme = {
        get: get,
        set: function (theme) { apply(theme, true); },
        toggle: function () { apply(get() === 'dark' ? 'light' : 'dark', true); },
        css: css,
        rgba: rgba,
        chart: chart
    };

    document.addEventListener('click', function (e) {
        var el = e.target && e.target.closest && e.target.closest('[data-theme-toggle]');
        if (el) window.VulcanTheme.toggle();
    });

    // Keep several open dashboard tabs in step.
    window.addEventListener('storage', function (e) {
        if (e.key === KEY && (e.newValue === 'dark' || e.newValue === 'light')) {
            apply(e.newValue, false);
        }
    });
})();
