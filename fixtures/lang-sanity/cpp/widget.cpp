#include "widget.h"

namespace app {
namespace ui {

int Widget::count_ = 0;

Widget::Widget(int size) : size_(size) { ++count_; }

Widget::~Widget() { --count_; }

int Widget::area() const {
    return size_ * size_;
}

int Widget::draw() const {
    return size_ * 2;
}

int Widget::resize(int by) {
    size_ += by;
    return size_;
}

int Widget::resize(int w, int h) {
    size_ = w * h;
    return size_;
}

Widget& Widget::operator+=(int by) {
    resize(by);
    return *this;
}

int Widget::instances() { return count_; }

int Shape::describe() const {
    return area();
}

int build_default() {
    Widget w(21);
    return w.draw();
}

int label_of(const Shape &s) {
    return s.area();
}

}  // namespace ui
}  // namespace app
