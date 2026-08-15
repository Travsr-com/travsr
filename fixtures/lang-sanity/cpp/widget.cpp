#include "widget.h"

namespace app {

Widget::Widget(int size) : size_(size) {}

int Widget::draw() const {
    return size_ * 2;
}

int build_default() {
    Widget w(21);
    return w.draw();
}

}  // namespace app
