#include <stdio.h>
#include "math_util.h"

int main(void) {
    int total = add_numbers(2, 3);
    int scaled = scale_value(total, 4);
    printf("%d %d\n", total, scaled);
    return 0;
}
