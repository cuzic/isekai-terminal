package tools.isekai.terminal

import android.app.Application
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.lifecycle.HasDefaultViewModelProviderFactory
import androidx.lifecycle.ViewModel
import androidx.lifecycle.ViewModelProvider
import androidx.lifecycle.ViewModelStore
import androidx.lifecycle.ViewModelStoreOwner
import androidx.lifecycle.viewmodel.compose.LocalViewModelStoreOwner
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout

/**
 * Compose画面テスト用の[ViewModelStoreOwner]。画面が内部で`viewModel()`する
 * ViewModel(例: [SnippetListViewModel]/[ProfileListViewModel])を**テスト側で先に生成**し、
 * その`init{}`が`Dispatchers.IO`へ投げたDB読み込みの完了を[StateFlow]で待ってから
 * 画面を合成するために使う。
 *
 * 背景: `DeletableListViewModel.load()`は`viewModelScope.launch(Dispatchers.IO)`で
 * 一覧を読み込むが、この仕事はComposeテストのアイドリング判定(`waitForIdle()`)の
 * 追跡対象外なので、`setContent{}`→`waitForIdle()`直後にはまだ一覧が描画されていない
 * ことがある(CI 2026-10-06でSnippetListScreenTest/ScreenshotGalleryTestが実際に
 * flaky化)。ここでは画面合成前にViewModelの状態自体が読み込み済みであることを
 * イベント駆動([StateFlow.first])で保証するので、初回合成の時点で一覧が描画される。
 *
 * 画面側の`viewModel()`は既定キー(`ViewModelProvider`のDefaultKey + クラス名)で
 * このストアを引くため、[preload]で同じキーに登録したインスタンスがそのまま返る。
 */
internal class PreloadedViewModelStoreOwner(app: Application) :
    ViewModelStoreOwner, HasDefaultViewModelProviderFactory {
    override val viewModelStore = ViewModelStore()
    override val defaultViewModelProviderFactory: ViewModelProvider.Factory =
        ViewModelProvider.AndroidViewModelFactory(app) // getInstance()はプロセス内で最初のApplicationをキャッシュするので使わない(Robolectricはテストごとに別インスタンス)

    /** [VM]を生成(既に有れば再利用)してこのストアに登録する。 */
    inline fun <reified VM : ViewModel> preload(): VM =
        ViewModelProvider(this, defaultViewModelProviderFactory)[VM::class.java]

    /** [flow]が[predicate]を満たすまで待つ(IOスレッドでの読み込み完了待ち)。
     *  タイムアウトはハング防止のためだけの上限で、通常は即座に返る。 */
    fun <T> awaitState(flow: StateFlow<T>, predicate: (T) -> Boolean): T =
        runBlocking { withTimeout(10_000) { flow.first(predicate) } }

    /** [content]をこのストアを[LocalViewModelStoreOwner]として合成する。 */
    @Composable
    fun Provide(content: @Composable () -> Unit) {
        CompositionLocalProvider(LocalViewModelStoreOwner provides this, content = content)
    }

    fun clear() = viewModelStore.clear()
}
