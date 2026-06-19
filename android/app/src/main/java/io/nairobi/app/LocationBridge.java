package io.nairobi.app;

import android.Manifest;
import android.app.Activity;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.ActivityNotFoundException;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.net.Uri;
import android.os.Build;
import android.os.Handler;
import android.os.Looper;
import android.util.Log;

import java.util.ArrayList;
import java.util.List;

/**
 * Static bridge between the Rust core and the Android platform.
 *
 * Rust calls the public static methods (from arbitrary threads — every method
 * that touches the platform hops to the main looper internally where required)
 * and receives results through the two native callbacks, which Rust registers
 * at startup via JNI RegisterNatives (see app/src/glue.rs):
 *
 *   nativeOnLocation(lat, lng)   — each location fix          (DD)V
 *   nativeOnPermission(granted)  — permission outcome         (Z)V
 *
 * The bridge deliberately takes no Context/Activity parameters: the
 * android-activity glue only exposes the Application context to native code,
 * and passing it where an Activity is expected trips CheckJNI. The live
 * activity is always resolved via {@link MainActivity#current()}.
 */
public final class LocationBridge {
    private static final String TAG = "nairobi";
    private static final int REQ_LOCATION = 4242;
    /** High-importance channel for match / status notifications. */
    private static final String ALERT_CHANNEL_ID = "nairobi.alert";
    /** Rolling id so successive notifications stack rather than overwrite. */
    private static int alertNotificationId = 100;

    private LocationBridge() {}

    // ---- native callbacks INTO Rust (registered from Rust) ----------------

    /** A new location fix. Registered by Rust as sig {@code (DD)V}. */
    static native void nativeOnLocation(double lat, double lng);

    /** The runtime permission request resolved. Registered as {@code (Z)V}. */
    static native void nativeOnPermission(boolean granted);

    /** The system back gesture/button. Registered by Rust as sig {@code ()V}. */
    static native void nativeOnBack();

    // ---- context resolution -----------------------------------------------

    /**
     * The Context used to drive location and check permissions: the live
     * activity while the app is open, else the foreground location service.
     * Both are Contexts; only an activity can host a permission dialog (see
     * {@link #requestLocationPermission}).
     */
    private static Context locationContext() {
        Activity activity = MainActivity.current();
        if (activity != null) return activity;
        return LocationService.current();
    }

    /** Post onto the main looper — works from any context, unlike an activity's
     * runOnUiThread. */
    private static void post(Runnable r) {
        new Handler(Looper.getMainLooper()).post(r);
    }

    private static void reportPermission(boolean granted) {
        try {
            nativeOnPermission(granted);
        } catch (UnsatisfiedLinkError e) {
            Log.e(TAG, "native callbacks not registered yet", e);
        }
    }

    // ---- permissions -------------------------------------------------------

    private static boolean hasForeground(Context ctx) {
        // Android 12+ lets users grant approximate location only; coarse fixes
        // are still useful for ride matching.
        return ctx.checkSelfPermission(Manifest.permission.ACCESS_FINE_LOCATION)
                        == PackageManager.PERMISSION_GRANTED
                || ctx.checkSelfPermission(Manifest.permission.ACCESS_COARSE_LOCATION)
                        == PackageManager.PERMISSION_GRANTED;
    }

    /** Whether we hold (fine or coarse) location permission. */
    public static boolean hasLocationPermission() {
        Context ctx = locationContext();
        return ctx != null && hasForeground(ctx);
    }

    /**
     * Request the runtime permissions ride tracking needs: fine + coarse
     * location, plus POST_NOTIFICATIONS on API 33+ so the foreground-service
     * notification is visible. The result flows back through
     * {@link MainActivity#onRequestPermissionsResult} → {@link
     * #handlePermissionResult} → {@link #nativeOnPermission}. Only an activity
     * can show these prompts.
     */
    public static void requestLocationPermission() {
        final Activity activity = MainActivity.current();
        if (activity == null) {
            Log.w(TAG, "requestLocationPermission: no live activity");
            reportPermission(false);
            return;
        }
        activity.runOnUiThread(() -> {
            if (hasForeground(activity)) {
                reportPermission(true);
                return;
            }
            List<String> perms = new ArrayList<>();
            perms.add(Manifest.permission.ACCESS_FINE_LOCATION);
            perms.add(Manifest.permission.ACCESS_COARSE_LOCATION);
            if (Build.VERSION.SDK_INT >= 33) {
                // Needed so the foreground-service notification is visible.
                perms.add(Manifest.permission.POST_NOTIFICATIONS);
            }
            activity.requestPermissions(perms.toArray(new String[0]), REQ_LOCATION);
        });
    }

    /** Called by {@link MainActivity} with a permission dialog outcome. */
    public static void handlePermissionResult(int requestCode, String[] permissions, int[] results) {
        if (requestCode == REQ_LOCATION) {
            reportPermission(hasLocationPermission());
        }
    }

    // ---- location updates --------------------------------------------------

    /**
     * Start delivering location updates. Brings up the foreground {@link
     * LocationService} (which keeps the process and GPS alive while the app is
     * backgrounded and a ride is matched) and subscribes it to the platform
     * LocationManager at {@code intervalMs}; each fix is forwarded to Rust via
     * {@link #nativeOnLocation}.
     */
    public static void startLocation(final long intervalMs) {
        final Context ctx = locationContext();
        if (ctx == null) {
            Log.w(TAG, "startLocation: no context");
            reportPermission(false);
            return;
        }
        final boolean fromActivity = ctx instanceof Activity;
        post(() -> {
            if (!hasLocationPermission()) {
                Log.w(TAG, "startLocation without permission");
                reportPermission(false);
                return;
            }
            // The UI path brings the keep-alive service up; if we are already
            // running inside the service it is up already.
            if (fromActivity) {
                try {
                    Intent svc = new Intent(ctx, LocationService.class);
                    svc.putExtra(LocationService.EXTRA_INTERVAL_MS, intervalMs);
                    ctx.startForegroundService(svc);
                } catch (Exception e) {
                    // The app can still track while in the foreground.
                    Log.e(TAG, "failed to start foreground service", e);
                    LocationService.subscribe(ctx, intervalMs);
                }
            } else {
                LocationService.subscribe(ctx, intervalMs);
            }
        });
    }

    /** Stop delivering location updates and tear the foreground service down. */
    public static void stopLocation() {
        final Context ctx = locationContext();
        if (ctx == null) return;
        post(() -> {
            LocationService.unsubscribe(ctx);
            ctx.stopService(new Intent(ctx, LocationService.class));
        });
    }

    // ---- back navigation ---------------------------------------------------

    /**
     * Forward a system back press to Rust, which decides whether to navigate to
     * the previous screen or (only from Home) exit via {@link #finishActivity}.
     * The activity always consumes the event, so if native is not yet ready we
     * fall back to finishing the activity rather than trapping the user.
     */
    static void dispatchBack() {
        try {
            nativeOnBack();
        } catch (UnsatisfiedLinkError e) {
            Log.e(TAG, "native back not ready; finishing activity", e);
            finishActivity();
        }
    }

    /** Close the app — called by Rust when back is pressed on the Home screen. */
    public static void finishActivity() {
        final Activity activity = MainActivity.current();
        if (activity == null) return;
        activity.runOnUiThread(activity::finish);
    }

    /** Forwarded by {@link LocationService} for each location fix. */
    static void deliverLocation(double lat, double lng) {
        try {
            nativeOnLocation(lat, lng);
        } catch (UnsatisfiedLinkError e) {
            Log.e(TAG, "native not ready for location", e);
        }
    }

    // ---- navigation hand-off ----------------------------------------------

    /**
     * Launch an external navigation app at ({@code lat},{@code lng}) — the
     * driver's NAVIGATE hand-off. Falls back to an OpenStreetMap web link when
     * no geo-capable app is installed.
     */
    public static void openNav(final double lat, final double lng, final String label) {
        final Context ctx = locationContext();
        if (ctx == null) {
            Log.w(TAG, "openNav: no context");
            return;
        }
        post(() -> {
            String coords = lat + "," + lng;
            Uri geo = Uri.parse("geo:" + coords + "?q=" + coords
                    + "(" + Uri.encode(label == null ? "destination" : label) + ")");
            Intent view = new Intent(Intent.ACTION_VIEW, geo)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
            try {
                ctx.startActivity(view);
            } catch (ActivityNotFoundException e) {
                Uri web = Uri.parse("https://www.openstreetmap.org/?mlat=" + lat
                        + "&mlon=" + lng + "#map=16/" + lat + "/" + lng);
                try {
                    ctx.startActivity(new Intent(Intent.ACTION_VIEW, web)
                            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));
                } catch (ActivityNotFoundException e2) {
                    Log.e(TAG, "no app can open a map or browser");
                }
            }
        });
    }

    /**
     * Open a web URL in the system browser — used to inspect a proof-of-burn
     * notarization transaction on a block explorer (e.g. mempool.emzy.de).
     */
    public static void openUrl(final String url) {
        final Context ctx = locationContext();
        if (ctx == null) {
            Log.w(TAG, "openUrl: no context");
            return;
        }
        post(() -> {
            try {
                Intent view = new Intent(Intent.ACTION_VIEW, Uri.parse(url))
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
                ctx.startActivity(view);
            } catch (Exception e) {
                Log.e(TAG, "openUrl failed: " + e);
            }
        });
    }

    // ---- clipboard ---------------------------------------------------------

    /**
     * Copy {@code text} to the system clipboard — lets the user paste a Lightning
     * invoice or on-chain deposit address into another wallet to fund this one.
     */
    public static void copyToClipboard(final String text) {
        final Context ctx = locationContext();
        if (ctx == null) {
            Log.w(TAG, "copyToClipboard: no context");
            return;
        }
        post(() -> {
            try {
                android.content.ClipboardManager cm =
                        ctx.getSystemService(android.content.ClipboardManager.class);
                if (cm == null) return;
                cm.setPrimaryClip(
                        android.content.ClipData.newPlainText("nairobi", text));
            } catch (Exception e) {
                Log.e(TAG, "copyToClipboard failed", e);
            }
        });
    }

    // ---- notifications -----------------------------------------------------

    /**
     * Post a notification — signals a ride match (or status change) — visible
     * even when the app is backgrounded. Tapping it opens the app. Works from
     * either the live activity or the foreground service, whichever {@link
     * #locationContext} resolves.
     */
    public static void notify(final String title, final String body) {
        final Context ctx = locationContext();
        if (ctx == null) {
            Log.w(TAG, "notify: no context");
            return;
        }
        post(() -> {
            try {
                NotificationManager nm = ctx.getSystemService(NotificationManager.class);
                if (nm == null) return;
                if (Build.VERSION.SDK_INT >= 26) {
                    NotificationChannel channel = new NotificationChannel(
                            ALERT_CHANNEL_ID, "Ride updates",
                            NotificationManager.IMPORTANCE_HIGH);
                    channel.setDescription("Ride matches and status updates");
                    channel.enableVibration(true);
                    nm.createNotificationChannel(channel);
                }

                Intent open = new Intent(ctx, MainActivity.class)
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
                PendingIntent tap = PendingIntent.getActivity(
                        ctx, 0, open,
                        PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);

                Notification n = new Notification.Builder(ctx, ALERT_CHANNEL_ID)
                        .setContentTitle(title)
                        .setContentText(body)
                        .setStyle(new Notification.BigTextStyle().bigText(body))
                        .setSmallIcon(android.R.drawable.ic_dialog_info)
                        .setAutoCancel(true)
                        .setContentIntent(tap)
                        .build();
                nm.notify(nextAlertId(), n);
            } catch (Exception e) {
                Log.e(TAG, "notify failed", e);
            }
        });
    }

    private static synchronized int nextAlertId() {
        return alertNotificationId++;
    }
}
