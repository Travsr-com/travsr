#include <stdio.h>
#include "math_util.h"

int main(void) {
    int total = add_numbers(2, 3);
    int scaled = scale_value(total, 4);
    int viaptr = apply_op(add_numbers, 1, 2);
    struct Point p = {1, 2};
    PointAlias q = {3, 4};
    int psum = point_sum(&p) + point_sum(&q);
    int w = enum_width(GREEN);
    int d = DOUBLE_IT(total);
    printf("%d %d %d %d %d %d\n", total, scaled, viaptr, psum, w, d);
    return 0;
}
