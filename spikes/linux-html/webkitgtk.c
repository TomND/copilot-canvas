/* Linux HTML view spike (docs/MAC-LINUX-PLAN.md §5.7, D9): WebKitGTK 4.1
 * rendered offscreen (a GtkOffscreenWindow, no surface on screen), the way the
 * GUI would draw it as a texture on X11 and Wayland alike. Locked down as the
 * plan says (JavaScript markup off, a content rule list blocking every load
 * with an address, navigation refused, no new windows, permissions denied,
 * downloads cancelled, ephemeral storage, no developer extras, no context
 * menu) and checked the way parity row 41b checks WebView2: evaluates the
 * page, counts what reached a local listener, forwards a scroll and a click.
 *
 *   webkitgtk OUTDIR WIDTH HEIGHT [--js] [--sandbox] [--no-filter] [--proxy]
 *
 * Reads OUTDIR/{brief,parity}.doc.html (sanitized) and OUTDIR/raw.html, writes
 * PNGs to OUTDIR and one JSON object to stdout. */
#include <webkit2/webkit2.h>
#include <gtk/gtk.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <dirent.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>

static double now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}

static gboolean set_true(gpointer p) {
    *(gboolean *)p = TRUE;
    return G_SOURCE_REMOVE;
}

/* Runs the main loop until *flag or ms elapse; answers *flag. */
static gboolean spin(volatile gboolean *flag, int ms) {
    gboolean timed_out = FALSE;
    guint id = g_timeout_add(ms, set_true, &timed_out);
    while (!*flag && !timed_out) g_main_context_iteration(NULL, TRUE);
    if (!timed_out) g_source_remove(id);
    return *flag;
}

static void sleep_ms(int ms) {
    gboolean never = FALSE;
    spin(&never, ms);
}

/* ---- the "network": a listener the page must never reach, and a proxy that answers 403 ---- */
typedef struct { int fd; int hits; GString *first; } Listener;

static gboolean on_accept(GIOChannel *ch, GIOCondition c, gpointer p) {
    (void)ch; (void)c;
    Listener *l = p;
    int s = accept(l->fd, NULL, NULL);
    if (s >= 0) {
        char buf[512] = {0};
        ssize_t n = recv(s, buf, sizeof buf - 1, 0);
        if (n > 0 && l->first->len == 0) {
            char *eol = strstr(buf, "\r\n");
            if (eol) *eol = 0;
            g_string_append(l->first, buf);
        }
        const char *r = "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        send(s, r, strlen(r), 0);
        close(s);
        l->hits++;
    }
    return TRUE;
}

static Listener *listen_on(int port) {
    Listener *l = g_new0(Listener, 1);
    l->first = g_string_new(NULL);
    l->fd = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(l->fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in a = {0};
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (bind(l->fd, (struct sockaddr *)&a, sizeof a) != 0 || listen(l->fd, 16) != 0) {
        fprintf(stderr, "listen %d failed\n", port);
        exit(2);
    }
    GIOChannel *ch = g_io_channel_unix_new(l->fd);
    g_io_add_watch(ch, G_IO_IN, on_accept, l);
    return l;
}

/* ---- memory: proportional set size of this process and every descendant (WebKit's processes) ---- */
static long pss_kb(int pid) {
    char path[64], line[256];
    snprintf(path, sizeof path, "/proc/%d/smaps_rollup", pid);
    FILE *f = fopen(path, "r");
    if (!f) return 0;
    long kb = 0;
    while (fgets(line, sizeof line, f))
        if (strncmp(line, "Pss:", 4) == 0) kb = atol(line + 4);
    fclose(f);
    return kb;
}

static int parent_of(int pid, char *name, size_t len) {
    char path[64], buf[512];
    snprintf(path, sizeof path, "/proc/%d/stat", pid);
    FILE *f = fopen(path, "r");
    if (!f) return -1;
    size_t n = fread(buf, 1, sizeof buf - 1, f);
    fclose(f);
    buf[n] = 0;
    char *open = strchr(buf, '('), *close_ = strrchr(buf, ')');
    if (!open || !close_) return -1;
    size_t nl = (size_t)(close_ - open - 1);
    if (nl >= len) nl = len - 1;
    memcpy(name, open + 1, nl);
    name[nl] = 0;
    int ppid = 0;
    sscanf(close_ + 2, "%*c %d", &ppid);
    return ppid;
}

static void memory(GString *out) {
    int me = getpid();
    long total = pss_kb(me);
    g_string_append_printf(out, "{\"self_kb\":%ld,\"children\":[", total);
    /* two passes find grandchildren too (WebKit's helpers can be spawned through bwrap) */
    int pids[64], n = 0;
    pids[n++] = me;
    for (int pass = 0; pass < 3; pass++) {
        DIR *d = opendir("/proc");
        struct dirent *e;
        while (d && (e = readdir(d))) {
            int pid = atoi(e->d_name);
            if (pid <= 0) continue;
            char name[64];
            int pp = parent_of(pid, name, sizeof name);
            gboolean known = FALSE, child = FALSE;
            for (int i = 0; i < n; i++) {
                if (pids[i] == pid) known = TRUE;
                if (pids[i] == pp) child = TRUE;
            }
            if (!known && child && n < 64) {
                pids[n++] = pid;
                long kb = pss_kb(pid);
                total += kb;
                g_string_append_printf(out, "%s{\"name\":\"%s\",\"kb\":%ld}", n > 2 ? "," : "", name, kb);
            }
        }
        if (d) closedir(d);
    }
    g_string_append_printf(out, "],\"total_kb\":%ld}", total);
}

/* ---- the view ---- */
typedef struct {
    GtkWidget *win, *view;
    gboolean finished, failed, damaged;
    int damages, loads;
    gboolean allow_next;           /* our own load_html is in flight */
    GPtrArray *blocked_navigations, *links, *resources, *notes;
    char *terminated;
} View;

static void note(View *v, const char *fmt, const char *arg) {
    g_ptr_array_add(v->notes, g_strdup_printf(fmt, arg ? arg : ""));
}

static gboolean on_damage(GtkWidget *w, GdkEvent *e, gpointer p) {
    (void)w; (void)e;
    View *v = p;
    v->damages++;
    v->damaged = TRUE;
    return FALSE;
}

static void on_load_changed(WebKitWebView *wv, WebKitLoadEvent ev, gpointer p) {
    (void)wv;
    View *v = p;
    if (ev == WEBKIT_LOAD_FINISHED) {
        v->finished = TRUE;
        v->loads++;
    }
}

static gboolean on_load_failed(WebKitWebView *wv, WebKitLoadEvent ev, gchar *uri, GError *err, gpointer p) {
    (void)wv; (void)ev;
    View *v = p;
    char *s = g_strdup_printf("load-failed %s: %s", uri, err ? err->message : "");
    note(v, "%s", s);
    g_free(s);
    return FALSE;
}

static gboolean on_decide(WebKitWebView *wv, WebKitPolicyDecision *d, WebKitPolicyDecisionType type, gpointer p) {
    (void)wv;
    View *v = p;
    if (type == WEBKIT_POLICY_DECISION_TYPE_RESPONSE) return FALSE;
    WebKitNavigationAction *a = webkit_navigation_policy_decision_get_navigation_action(WEBKIT_NAVIGATION_POLICY_DECISION(d));
    const char *uri = webkit_uri_request_get_uri(webkit_navigation_action_get_request(a));
    WebKitNavigationType nt = webkit_navigation_action_get_navigation_type(a);
    if (type == WEBKIT_POLICY_DECISION_TYPE_NAVIGATION_ACTION && v->allow_next && (g_str_has_prefix(uri, "about:blank") || g_str_has_prefix(uri, "data:"))) {
        v->allow_next = FALSE;
        webkit_policy_decision_use(d);
        return TRUE;
    }
    if (nt == WEBKIT_NAVIGATION_TYPE_LINK_CLICKED && webkit_navigation_action_is_user_gesture(a))
        g_ptr_array_add(v->links, g_strdup(uri));
    else
        g_ptr_array_add(v->blocked_navigations, g_strdup(uri));
    webkit_policy_decision_ignore(d);
    return TRUE;
}

static GtkWidget *on_create(WebKitWebView *wv, WebKitNavigationAction *a, gpointer p) {
    (void)wv;
    View *v = p;
    note(v, "create refused: %s", webkit_uri_request_get_uri(webkit_navigation_action_get_request(a)));
    return NULL;
}

static gboolean on_permission(WebKitWebView *wv, WebKitPermissionRequest *r, gpointer p) {
    (void)wv; (void)p;
    webkit_permission_request_deny(r);
    return TRUE;
}

static gboolean on_context_menu(WebKitWebView *wv, WebKitContextMenu *m, GdkEvent *e, WebKitHitTestResult *h, gpointer p) {
    (void)wv; (void)m; (void)e; (void)h; (void)p;
    return TRUE;
}

static void on_terminated(WebKitWebView *wv, WebKitWebProcessTerminationReason r, gpointer p) {
    (void)wv;
    View *v = p;
    v->terminated = g_strdup_printf("%d", (int)r);
}

static void on_resource(WebKitWebView *wv, WebKitWebResource *res, WebKitURIRequest *req, gpointer p) {
    (void)wv; (void)res;
    View *v = p;
    g_ptr_array_add(v->resources, g_strdup(webkit_uri_request_get_uri(req)));
}

static void on_download(WebKitWebContext *c, WebKitDownload *d, gpointer p) {
    (void)c; (void)p;
    webkit_download_cancel(d);
}

/* content filter */
typedef struct { gboolean done; WebKitUserContentFilter *filter; char *error; } FilterSave;
static void on_filter_saved(GObject *src, GAsyncResult *res, gpointer p) {
    FilterSave *s = p;
    GError *err = NULL;
    s->filter = webkit_user_content_filter_store_save_finish(WEBKIT_USER_CONTENT_FILTER_STORE(src), res, &err);
    if (err) s->error = g_strdup(err->message);
    s->done = TRUE;
}

/* evaluate */
typedef struct { gboolean done; char *value; } Eval;
static void on_eval(GObject *src, GAsyncResult *res, gpointer p) {
    Eval *e = p;
    GError *err = NULL;
    JSCValue *val = webkit_web_view_evaluate_javascript_finish(WEBKIT_WEB_VIEW(src), res, &err);
    if (err) e->value = g_strdup_printf("ERROR: %s", err->message);
    else if (val) e->value = jsc_value_to_string(val);
    else e->value = g_strdup("null");
    e->done = TRUE;
}

static char *eval(View *v, const char *script) {
    Eval e = {0};
    webkit_web_view_evaluate_javascript(WEBKIT_WEB_VIEW(v->view), script, -1, NULL, NULL, NULL, on_eval, &e);
    if (!spin(&e.done, 5000)) return g_strdup("ERROR: timeout");
    return e.value;
}

/* snapshot */
typedef struct { gboolean done; cairo_surface_t *s; char *error; } Snap;
static void on_snap(GObject *src, GAsyncResult *res, gpointer p) {
    Snap *s = p;
    GError *err = NULL;
    s->s = webkit_web_view_get_snapshot_finish(WEBKIT_WEB_VIEW(src), res, &err);
    if (err) s->error = g_strdup(err->message);
    s->done = TRUE;
}

/* Loads html, waits for the load and the first frame after it; answers ms to the frame. */
static double load(View *v, const char *html) {
    double t = now_ms();
    v->finished = FALSE;
    v->allow_next = TRUE;
    webkit_web_view_load_html(WEBKIT_WEB_VIEW(v->view), html, "about:blank");
    if (!spin(&v->finished, 15000)) return -1;
    v->damaged = FALSE;
    spin(&v->damaged, 3000);
    double frame = now_ms() - t;
    /* let painting settle (images decode, fonts) */
    int before;
    do {
        before = v->damages;
        sleep_ms(150);
    } while (v->damages != before);
    return frame;
}

static double capture(View *v, const char *path) {
    double t = now_ms();
    GdkPixbuf *pb = gtk_offscreen_window_get_pixbuf(GTK_OFFSCREEN_WINDOW(v->win));
    if (!pb) return -1;
    gdk_pixbuf_save(pb, path, "png", NULL, NULL);
    g_object_unref(pb);
    return now_ms() - t;
}

static double snapshot(View *v, const char *path, GString *err) {
    double t = now_ms();
    Snap s = {0};
    webkit_web_view_get_snapshot(WEBKIT_WEB_VIEW(v->view), WEBKIT_SNAPSHOT_REGION_VISIBLE, WEBKIT_SNAPSHOT_OPTIONS_NONE, NULL, on_snap, &s);
    if (!spin(&s.done, 5000) || !s.s) {
        g_string_append(err, s.error ? s.error : "timeout");
        return -1;
    }
    double ms = now_ms() - t;
    cairo_surface_write_to_png(s.s, path);
    cairo_surface_destroy(s.s);
    return ms;
}

static void send_scroll(View *v, int x, int y, double dy) {
    GdkWindow *w = gtk_widget_get_window(v->view);
    GdkEvent *ev = gdk_event_new(GDK_SCROLL);
    ev->scroll.window = g_object_ref(w);
    ev->scroll.send_event = TRUE;
    ev->scroll.time = GDK_CURRENT_TIME;
    ev->scroll.x = x;
    ev->scroll.y = y;
    ev->scroll.direction = GDK_SCROLL_SMOOTH;
    ev->scroll.delta_x = 0;
    ev->scroll.delta_y = dy;
    GdkSeat *seat = gdk_display_get_default_seat(gdk_window_get_display(w));
    gdk_event_set_device(ev, gdk_seat_get_pointer(seat));
    gtk_widget_event(v->view, ev);
    gdk_event_free(ev);
}

static void send_click(View *v, int x, int y) {
    GdkWindow *w = gtk_widget_get_window(v->view);
    GdkSeat *seat = gdk_display_get_default_seat(gdk_window_get_display(w));
    GdkEventType types[] = { GDK_MOTION_NOTIFY, GDK_BUTTON_PRESS, GDK_BUTTON_RELEASE };
    for (int i = 0; i < 3; i++) {
        GdkEvent *ev = gdk_event_new(types[i]);
        if (types[i] == GDK_MOTION_NOTIFY) {
            ev->motion.window = g_object_ref(w);
            ev->motion.send_event = TRUE;
            ev->motion.time = GDK_CURRENT_TIME;
            ev->motion.x = x;
            ev->motion.y = y;
        } else {
            ev->button.window = g_object_ref(w);
            ev->button.send_event = TRUE;
            ev->button.time = GDK_CURRENT_TIME;
            ev->button.x = x;
            ev->button.y = y;
            ev->button.button = 1;
            if (types[i] == GDK_BUTTON_RELEASE) ev->button.state = GDK_BUTTON1_MASK;
        }
        gdk_event_set_device(ev, gdk_seat_get_pointer(seat));
        gtk_widget_event(v->view, ev);
        gdk_event_free(ev);
        sleep_ms(30);
    }
}

static char *read_file(const char *dir, const char *name) {
    char *path = g_build_filename(dir, name, NULL), *s = NULL;
    if (!g_file_get_contents(path, &s, NULL, NULL)) {
        fprintf(stderr, "missing %s\n", path);
        exit(2);
    }
    g_free(path);
    return s;
}

static void json_str(GString *o, const char *s) {
    g_string_append_c(o, '"');
    for (; s && *s; s++) {
        if (*s == '"' || *s == '\\') g_string_append_c(o, '\\');
        if ((unsigned char)*s < 0x20) g_string_append_printf(o, "\\u%04x", *s);
        else g_string_append_c(o, *s);
    }
    g_string_append_c(o, '"');
}

static void json_list(GString *o, GPtrArray *a) {
    g_string_append_c(o, '[');
    for (guint i = 0; i < a->len; i++) {
        if (i) g_string_append_c(o, ',');
        json_str(o, g_ptr_array_index(a, i));
    }
    g_string_append_c(o, ']');
}

int main(int argc, char **argv) {
    if (argc < 4) {
        fprintf(stderr, "usage: webkitgtk OUTDIR WIDTH HEIGHT [--js] [--sandbox] [--no-filter] [--proxy]\n");
        return 2;
    }
    const char *dir = argv[1];
    int width = atoi(argv[2]), height = atoi(argv[3]);
    gboolean js = FALSE, sandbox = FALSE, filter = TRUE, proxy = FALSE;
    for (int i = 4; i < argc; i++) {
        if (!strcmp(argv[i], "--js")) js = TRUE;
        if (!strcmp(argv[i], "--sandbox")) sandbox = TRUE;
        if (!strcmp(argv[i], "--no-filter")) filter = FALSE;
        if (!strcmp(argv[i], "--proxy")) proxy = TRUE;
    }
    GString *o = g_string_new("{");
    double t0 = now_ms();
    gtk_init(&argc, &argv);
    double t_gtk = now_ms() - t0;
    g_string_append_printf(o, "\"renderer\":\"webkitgtk-%u.%u.%u\",\"gdk_backend\":", webkit_get_major_version(), webkit_get_minor_version(), webkit_get_micro_version());
    json_str(o, G_OBJECT_TYPE_NAME(gdk_display_get_default()));
    g_string_append_printf(o, ",\"js_api\":%s,\"sandbox\":%s,\"filter\":%s,\"proxy\":%s,\"gtk_init_ms\":%.1f", js ? "true" : "false", sandbox ? "true" : "false", filter ? "true" : "false", proxy ? "true" : "false", t_gtk);

    Listener *target = listen_on(49231);
    Listener *deny_proxy = proxy ? listen_on(49232) : NULL;

    WebKitWebContext *ctx = webkit_web_context_new_ephemeral();
    if (sandbox) webkit_web_context_set_sandbox_enabled(ctx, TRUE);
    webkit_web_context_set_cache_model(ctx, WEBKIT_CACHE_MODEL_DOCUMENT_VIEWER);
    g_signal_connect(ctx, "download-started", G_CALLBACK(on_download), NULL);
    if (proxy) {
        WebKitNetworkProxySettings *ps = webkit_network_proxy_settings_new("http://127.0.0.1:49232", NULL);
        webkit_website_data_manager_set_network_proxy_settings(webkit_web_context_get_website_data_manager(ctx), WEBKIT_NETWORK_PROXY_MODE_CUSTOM, ps);
        webkit_network_proxy_settings_free(ps);
    }

    WebKitUserContentManager *ucm = webkit_user_content_manager_new();
    if (filter) {
        static const char rules[] = "["
            "{\"trigger\":{\"url-filter\":\"^https?:\"},\"action\":{\"type\":\"block\"}},"
            "{\"trigger\":{\"url-filter\":\"^wss?:\"},\"action\":{\"type\":\"block\"}},"
            "{\"trigger\":{\"url-filter\":\"^ftp:\"},\"action\":{\"type\":\"block\"}},"
            "{\"trigger\":{\"url-filter\":\"^file:\"},\"action\":{\"type\":\"block\"}},"
            "{\"trigger\":{\"url-filter\":\"^blob:\"},\"action\":{\"type\":\"block\"}}]";
        char *store_dir = g_dir_make_tmp("cc-filter-XXXXXX", NULL);
        WebKitUserContentFilterStore *store = webkit_user_content_filter_store_new(store_dir);
        FilterSave fs = {0};
        double tf = now_ms();
        GBytes *b = g_bytes_new_static(rules, strlen(rules));
        webkit_user_content_filter_store_save(store, "canvas-report", b, NULL, on_filter_saved, &fs);
        spin(&fs.done, 10000);
        g_string_append_printf(o, ",\"filter_compile_ms\":%.1f,\"filter_error\":", now_ms() - tf);
        json_str(o, fs.error ? fs.error : "");
        if (fs.filter) webkit_user_content_manager_add_filter(ucm, fs.filter);
    }

    WebKitSettings *s = webkit_settings_new();
    g_object_set(s,
                 "enable-javascript", js,
                 "enable-javascript-markup", FALSE,
                 "javascript-can-open-windows-automatically", FALSE,
                 "javascript-can-access-clipboard", FALSE,
                 "enable-developer-extras", FALSE,
                 "enable-page-cache", FALSE,
                 "enable-html5-local-storage", FALSE,
                 "enable-webgl", FALSE,
                 "enable-webaudio", FALSE,
                 "enable-media", FALSE,
                 "enable-fullscreen", FALSE,
                 "enable-back-forward-navigation-gestures", FALSE,
                 "allow-file-access-from-file-urls", FALSE,
                 "allow-universal-access-from-file-urls", FALSE,
                 "hardware-acceleration-policy", WEBKIT_HARDWARE_ACCELERATION_POLICY_NEVER,
                 NULL);

    View v = {0};
    v.blocked_navigations = g_ptr_array_new();
    v.links = g_ptr_array_new();
    v.resources = g_ptr_array_new();
    v.notes = g_ptr_array_new();
    v.view = GTK_WIDGET(g_object_new(WEBKIT_TYPE_WEB_VIEW, "web-context", ctx, "settings", s, "user-content-manager", ucm, NULL));
    g_signal_connect(v.view, "load-changed", G_CALLBACK(on_load_changed), &v);
    g_signal_connect(v.view, "load-failed", G_CALLBACK(on_load_failed), &v);
    g_signal_connect(v.view, "decide-policy", G_CALLBACK(on_decide), &v);
    g_signal_connect(v.view, "create", G_CALLBACK(on_create), &v);
    g_signal_connect(v.view, "permission-request", G_CALLBACK(on_permission), &v);
    g_signal_connect(v.view, "context-menu", G_CALLBACK(on_context_menu), &v);
    g_signal_connect(v.view, "web-process-terminated", G_CALLBACK(on_terminated), &v);
    gboolean has_resource_signal = g_signal_lookup("resource-load-started", WEBKIT_TYPE_WEB_VIEW) != 0;
    if (has_resource_signal) g_signal_connect(v.view, "resource-load-started", G_CALLBACK(on_resource), &v);
    g_string_append_printf(o, ",\"resource_load_started_signal\":%s", has_resource_signal ? "true" : "false");

    v.win = gtk_offscreen_window_new();
    gtk_window_set_default_size(GTK_WINDOW(v.win), width, height);
    gtk_widget_set_size_request(v.view, width, height);
    gtk_container_add(GTK_CONTAINER(v.win), v.view);
    g_signal_connect(v.win, "damage-event", G_CALLBACK(on_damage), &v);
    gtk_widget_show_all(v.win);
    g_string_append_printf(o, ",\"setup_ms\":%.1f", now_ms() - t0);

    /* 1. the realistic report: first frame, capture, snapshot, memory */
    char *brief = read_file(dir, "brief.doc.html");
    double first = load(&v, brief);
    g_string_append_printf(o, ",\"brief_first_frame_ms\":%.1f,\"since_start_ms\":%.1f", first, now_ms() - t0);
    char *p1 = g_build_filename(dir, "brief.webkitgtk.png", NULL), *p2 = g_build_filename(dir, "brief.snapshot.png", NULL);
    g_string_append_printf(o, ",\"capture_ms\":%.1f", capture(&v, p1));
    GString *serr = g_string_new(NULL);
    g_string_append_printf(o, ",\"snapshot_ms\":%.1f,\"snapshot_error\":", snapshot(&v, p2, serr));
    json_str(o, serr->str);
    g_string_append(o, ",\"memory_after_first\":");
    memory(o);
    double again = load(&v, brief);
    g_string_append_printf(o, ",\"brief_reload_ms\":%.1f", again);

    /* 2. the parity page (sanitized), checked like row 41b */
    char *parity = read_file(dir, "parity.doc.html");
    load(&v, parity);
    char *pp = g_build_filename(dir, "parity.webkitgtk.png", NULL);
    capture(&v, pp);
    char *seen = eval(&v, "JSON.stringify({title: document.title, h1: document.getElementById('t').textContent, scripts: document.scripts.length, images: Array.from(document.images).map(i => i.getAttribute('src')), color: getComputedStyle(document.getElementById('t')).color, csp: !!document.querySelector('meta[http-equiv=\"Content-Security-Policy\"]')})");
    g_string_append(o, ",\"parity_eval\":");
    json_str(o, seen);

    /* 3. the raw page: the view's own lock-down without the sanitizer */
    char *raw = read_file(dir, "raw.html");
    load(&v, raw);
    char *rp = g_build_filename(dir, "raw.webkitgtk.png", NULL);
    capture(&v, rp);
    char *raw_seen = eval(&v, "JSON.stringify({title: document.title, h1: document.getElementById('r').textContent, scripts: document.scripts.length, onerror: Array.from(document.images).map(i => i.getAttribute('onerror'))})");
    g_string_append(o, ",\"raw_eval\":");
    json_str(o, raw_seen);

    /* 4. a navigation as a script would start it is refused; the page stays */
    char *nav = eval(&v, "location.href = 'https://example.com/navigated'; 'asked'");
    sleep_ms(800);
    char *still = eval(&v, "document.getElementById('r') ? 'stayed' : 'left'");
    g_string_append(o, ",\"navigate_eval\":");
    json_str(o, nav);
    g_string_append(o, ",\"after_navigation\":");
    json_str(o, still);

    /* 5. input forwarded from the canvas: a wheel scroll and a click on the link */
    char *y0 = eval(&v, "String(window.scrollY)");
    double ts = now_ms();
    send_scroll(&v, width / 2, height / 2, 3.0);
    sleep_ms(400);
    char *y1 = eval(&v, "String(window.scrollY)");
    char *sp = g_build_filename(dir, "raw.scrolled.png", NULL);
    capture(&v, sp);
    g_string_append_printf(o, ",\"scroll\":{\"before\":\"%s\",\"after\":\"%s\",\"ms\":%.1f}", y0, y1, now_ms() - ts);
    eval(&v, "window.scrollTo(0, 0); 'top'");
    sleep_ms(200);
    char *rect = eval(&v, "(() => { const r = document.getElementById('l').getBoundingClientRect(); return Math.round(r.x + r.width / 2) + ',' + Math.round(r.y + r.height / 2); })()");
    int cx = 60, cy = 120;
    if (rect && strchr(rect, ',')) sscanf(rect, "%d,%d", &cx, &cy);
    send_click(&v, cx, cy);
    sleep_ms(800);
    g_string_append_printf(o, ",\"click_at\":\"%d,%d\"", cx, cy);

    /* what reached the network */
    sleep_ms(1000);
    g_string_append_printf(o, ",\"listener_hits\":%d,\"listener_first\":", target->hits);
    json_str(o, target->first->str);
    if (deny_proxy) {
        g_string_append_printf(o, ",\"proxy_403\":%d,\"proxy_first\":", deny_proxy->hits);
        json_str(o, deny_proxy->first->str);
    }
    g_string_append(o, ",\"blocked_navigations\":");
    json_list(o, v.blocked_navigations);
    g_string_append(o, ",\"links\":");
    json_list(o, v.links);
    g_string_append(o, ",\"resources\":");
    json_list(o, v.resources);
    g_string_append(o, ",\"notes\":");
    json_list(o, v.notes);
    g_string_append(o, ",\"web_process_terminated\":");
    json_str(o, v.terminated ? v.terminated : "");
    g_string_append_printf(o, ",\"loads\":%d,\"damages\":%d,\"memory_end\":", v.loads, v.damages);
    memory(o);
    g_string_append_printf(o, ",\"total_ms\":%.1f}", now_ms() - t0);
    printf("%s\n", o->str);
    fflush(stdout);
    return 0;
}
