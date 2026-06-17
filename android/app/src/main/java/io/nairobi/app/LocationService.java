package io.nairobi.app;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.location.Location;
import android.location.LocationListener;
import android.location.LocationManager;
import android.os.Build;
import android.os.Bundle;
import android.os.IBinder;
import android.os.Looper;
import android.util.Log;

import java.util.ArrayList;
import java.util.List;

/**
 * Foreground service shown while a ride is being tracked. It keeps the process
 * and location access alive with the screen off (so live location stays
 * published while a ride is matched), shows the user an ongoing notification,
 * and owns the {@link LocationManager} subscription — forwarding every fix to
 * Rust via {@link LocationBridge#deliverLocation}.
 *
 * It also exposes itself (via {@link #current}) as the Context {@link
 * LocationBridge} uses for location when no activity is present.
 */
public class LocationService extends Service {
    private static final String TAG = "nairobi";
    private static final String CHANNEL_ID = "nairobi.tracking";
    private static final int NOTIFICATION_ID = 1;

    /** Intent extra carrying the requested sampling interval (ms). */
    public static final String EXTRA_INTERVAL_MS = "interval_ms";

    /** GPS sampling interval (ms) while acquiring the first fix — short enough
     * to keep the radio powered (≈continuous tracking) rather than
     * duty-cycling, so a cold start can complete. */
    private static final long ACQUIRE_INTERVAL_MS = 1000L;

    private static volatile LocationService sInstance;

    private static LocationListener listener;
    /** Whether the current session has delivered at least one fix yet. */
    private static boolean acquiredFix;

    /** The live service instance, or null when not running. Used by
     * {@link LocationBridge} to drive location with no activity present. */
    static LocationService current() {
        return sInstance;
    }

    @Override
    public void onCreate() {
        super.onCreate();
        sInstance = this;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        NotificationManager nm = getSystemService(NotificationManager.class);
        if (Build.VERSION.SDK_INT >= 26) {
            NotificationChannel channel = new NotificationChannel(
                    CHANNEL_ID, "Live ride tracking", NotificationManager.IMPORTANCE_LOW);
            channel.setDescription("Shown while nairobi is tracking your ride");
            nm.createNotificationChannel(channel);
        }

        Intent open = new Intent(this, MainActivity.class);
        PendingIntent tap = PendingIntent.getActivity(
                this, 0, open, PendingIntent.FLAG_IMMUTABLE);

        Notification notification = new Notification.Builder(this, CHANNEL_ID)
                .setContentTitle("Ride in progress")
                .setContentText("Your live location is being shared with your match.")
                .setSmallIcon(android.R.drawable.ic_menu_mylocation)
                .setOngoing(true)
                .setContentIntent(tap)
                .build();

        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(NOTIFICATION_ID, notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_LOCATION);
        } else {
            startForeground(NOTIFICATION_ID, notification);
        }

        long intervalMs = intent != null
                ? intent.getLongExtra(EXTRA_INTERVAL_MS, ACQUIRE_INTERVAL_MS)
                : ACQUIRE_INTERVAL_MS;
        subscribe(this, intervalMs);

        return START_NOT_STICKY;
    }

    // ---- location subscription (main-thread only) -------------------------

    /**
     * (Re)subscribe the location listener at {@code intervalMs}, replacing any
     * existing subscription. Mutates the static {@link #listener}, so it must
     * run on the main looper — callers post here first.
     */
    static void subscribe(final Context ctx, final long intervalMs) {
        try {
            LocationManager lm =
                    (LocationManager) ctx.getSystemService(Context.LOCATION_SERVICE);
            unsubscribe(ctx);
            acquiredFix = false;
            listener = new LocationListener() {
                @Override
                public void onLocationChanged(Location location) {
                    LocationBridge.deliverLocation(
                            location.getLatitude(), location.getLongitude());
                    // First fix of the session: drop from the continuous
                    // acquisition rate to the requested cadence so the GPS can
                    // duty-cycle and save power. Re-subscribing from a callback
                    // must hop to the looper (it mutates `listener`); the
                    // !acquiredFix guard makes this fire once.
                    if (!acquiredFix) {
                        acquiredFix = true;
                        if (Math.max(intervalMs, 1000L) > ACQUIRE_INTERVAL_MS) {
                            new android.os.Handler(Looper.getMainLooper()).post(() -> {
                                if (listener != null) subscribe(ctx, intervalMs);
                            });
                        }
                    }
                }
                @Override public void onStatusChanged(String provider, int status, Bundle extras) {}
                @Override public void onProviderEnabled(String provider) {}
                @Override public void onProviderDisabled(String provider) {}
            };
            // Until the first fix lands, sample fast enough to keep the radio
            // continuously powered (ACQUIRE_INTERVAL_MS): a long minTime lets
            // Android rest the GPS between windows, stalling a cold start.
            long minTimeMs = acquiredFix ? Math.max(intervalMs, 1000L) : ACQUIRE_INTERVAL_MS;
            boolean any = false;
            for (String provider : pickProviders(lm)) {
                lm.requestLocationUpdates(provider, minTimeMs, 0f,
                        listener, Looper.getMainLooper());
                any = true;
                Log.i(TAG, "location updates from " + provider + " every " + minTimeMs + "ms");
                Location last = lm.getLastKnownLocation(provider);
                if (last != null
                        && System.currentTimeMillis() - last.getTime() < 60_000) {
                    listener.onLocationChanged(last);
                }
            }
            if (!any) {
                Log.w(TAG, "no usable location provider");
            }
        } catch (SecurityException e) {
            Log.e(TAG, "location permission lost", e);
        }
    }

    private static List<String> pickProviders(LocationManager lm) {
        List<String> out = new ArrayList<>();
        List<String> all = lm.getAllProviders();
        if (Build.VERSION.SDK_INT >= 31 && all.contains(LocationManager.FUSED_PROVIDER)) {
            out.add(LocationManager.FUSED_PROVIDER);
            return out;
        }
        if (all.contains(LocationManager.GPS_PROVIDER)) out.add(LocationManager.GPS_PROVIDER);
        if (all.contains(LocationManager.NETWORK_PROVIDER)) out.add(LocationManager.NETWORK_PROVIDER);
        return out;
    }

    /** Remove the active location subscription, if any. Main-thread only. */
    static void unsubscribe(Context ctx) {
        if (listener != null) {
            LocationManager lm =
                    (LocationManager) ctx.getSystemService(Context.LOCATION_SERVICE);
            try {
                lm.removeUpdates(listener);
            } catch (Exception e) {
                Log.w(TAG, "removeUpdates failed", e);
            }
            listener = null;
        }
        acquiredFix = false;
    }

    @Override
    public void onDestroy() {
        unsubscribe(this);
        if (sInstance == this) {
            sInstance = null;
        }
        super.onDestroy();
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
