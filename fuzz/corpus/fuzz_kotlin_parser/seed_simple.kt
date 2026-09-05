package com.acme.app

import kotlin.math.max

interface Greeter {
    fun greet(name: String): String
}

data class Hello(val prefix: String) : Greeter {
    override fun greet(name: String): String = "$prefix $name"
}

fun main() {
    println(Hello("hello").greet("world"))
}
