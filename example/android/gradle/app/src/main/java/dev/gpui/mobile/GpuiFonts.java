package dev.gpui.mobile;

import android.content.Context;
import android.content.res.Configuration;
import android.content.res.Resources;
import android.content.res.TypedArray;
import android.graphics.Paint;
import android.graphics.Typeface;
import android.graphics.fonts.Font;
import android.graphics.text.PositionedGlyphs;
import android.graphics.text.TextRunShaper;
import android.os.Build;
import android.util.DisplayMetrics;
import android.util.Log;
import android.util.TypedValue;

import java.io.File;
import java.util.LinkedHashSet;

/**
 * What the system says of its text, for gpui-mobile's text system
 * ({@code src/system_fonts.rs} there reads the answer): how large and how
 * heavy the person wants text, and the files of the font the system draws
 * its own text with.
 *
 * <p>The font is not guessed from a list of names: the system is asked to
 * shape a letter in the device's own text appearance at every weight, and
 * says which file it drew it from. So it is Google Sans on a Pixel, the
 * maker's own font on other phones, and the font the person picked where
 * the maker lets them, as long as it is a file the application may read.
 */
public final class GpuiFonts {
    private static final String TAG = "GpuiFonts";

    private GpuiFonts() {}

    /**
     * Lines of tab-separated fields: {@code scale <factor>},
     * {@code weight <adjustment>}, then {@code face <path> <index>} for each
     * file of the system's font, the regular one first.
     */
    public static String describe(Context context) {
        StringBuilder out = new StringBuilder();
        try {
            Resources resources = context.getResources();
            Configuration config = resources.getConfiguration();
            DisplayMetrics metrics = resources.getDisplayMetrics();
            // Text at body size, in sp over dp: the font size setting, along
            // the curve Android 14 draws large text by.
            float sp = TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_SP, 16f, metrics);
            float dp = TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, 16f, metrics);
            float scale = dp > 0f ? sp / dp : config.fontScale;
            out.append("scale\t").append(scale).append('\n');
            if (Build.VERSION.SDK_INT >= 31) {
                int adjustment = config.fontWeightAdjustment;
                if (adjustment != Configuration.FONT_WEIGHT_ADJUSTMENT_UNDEFINED) {
                    out.append("weight\t").append(adjustment).append('\n');
                }
                faces(context, out);
            }
        } catch (Throwable error) {
            Log.w(TAG, "the system's text could not be read", error);
        }
        return out.toString();
    }

    /** The files the device's text appearance draws from, regular first. */
    @android.annotation.TargetApi(31)
    private static void faces(Context context, StringBuilder out) {
        Typeface base = Typeface.DEFAULT;
        TypedArray attributes = context.obtainStyledAttributes(
                android.R.style.TextAppearance_DeviceDefault,
                new int[] { android.R.attr.fontFamily });
        try {
            String family = attributes.getString(0);
            if (family != null && !family.isEmpty()) {
                base = Typeface.create(family, Typeface.NORMAL);
            }
        } finally {
            attributes.recycle();
        }

        int[] weights = { 400, 100, 200, 300, 500, 600, 700, 800, 900 };
        LinkedHashSet<String> files = new LinkedHashSet<>();
        for (boolean italic : new boolean[] { false, true }) {
            for (int weight : weights) {
                Paint paint = new Paint();
                paint.setTypeface(Typeface.create(base, weight, italic));
                PositionedGlyphs glyphs =
                        TextRunShaper.shapeTextRun("a", 0, 1, 0, 1, 0f, 0f, false, paint);
                if (glyphs.glyphCount() == 0) {
                    continue;
                }
                Font font = glyphs.getFont(0);
                File file = font.getFile();
                if (file == null) {
                    continue;
                }
                files.add(file.getAbsolutePath() + "\t" + font.getTtcIndex());
            }
        }
        for (String file : files) {
            out.append("face\t").append(file).append('\n');
        }
    }
}
