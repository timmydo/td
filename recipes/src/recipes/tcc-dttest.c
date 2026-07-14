/* Runtime double->integer conversion self-test for the tcc rung (re #469).
 *
 * On i386, tcc lowers (int)double and (long long)double to calls into
 * libtcc1's __fixdfdi / __fixunsdfdi soft-float helpers, whose mantissa mask
 *   #define HIDDEND_LL ((long long)1 << 52)
 * is a 64-bit constant left-shift across bit 32. A tcc generation that
 * mis-folds that shift (shift count lands in the high dword) ships a helper
 * that returns 0 for every conversion. `volatile` defeats compile-time folding
 * so the RUNTIME helper is exercised; the separate compile-time (int)5.0 fold
 * bug is intentionally NOT tested here.
 *
 * Compiled and run by the rung's own tcc, linked against the final libtcc1,
 * under kaem --strict: exits 0 iff every conversion is correct, 1 otherwise, so
 * the rung goes red if the shipped __fixdfdi ever regresses.
 */
int main(void)
{
  volatile double v5 = (double)5;
  volatile double v8 = (double)3 + (double)5;
  int d2i5 = (int)v5;
  int d2i8 = (int)v8;
  long long d2ll5 = (long long)v5;
  long long d2ll8 = (long long)v8;

  if (d2i5 != 5)
    return 1;
  if (d2i8 != 8)
    return 1;
  if (d2ll5 != 5)
    return 1;
  if (d2ll8 != 8)
    return 1;
  return 0;
}
